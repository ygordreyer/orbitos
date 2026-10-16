//! Theme options resolve before any assets or fonts are prepared.
use super::pack::{Pack, valid_id};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub type Values = BTreeMap<String, String>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setting {
    pub id: String,
    pub label: String,
    pub default: String,
    pub options: Vec<OptionSpec>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionSpec {
    pub id: String,
    pub label: String,
    pub patch: Value,
}
fn setting_id(id: &str) -> bool {
    valid_id(&id.replace('_', "-"))
}
fn label(label: &str) -> bool {
    !label.trim().is_empty() && label.len() <= 80 && !label.chars().any(char::is_control)
}

/// RFC 7386: null removes an object member and arrays replace as a unit.
pub fn merge_patch(target: &mut Value, patch: &Value) {
    if let Value::Object(patch) = patch {
        if !target.is_object() {
            *target = Value::Object(Default::default());
        }
        let target = target.as_object_mut().unwrap();
        for (key, value) in patch {
            if value.is_null() {
                target.remove(key);
            } else {
                merge_patch(target.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
    } else {
        *target = patch.clone();
    }
}

pub(super) fn validate(raw: &Value) -> Result<()> {
    let pack = raw["id"].as_str().unwrap_or("<unknown>");
    let Some(settings) = raw.get("settings") else {
        return Ok(());
    };
    let settings = settings.as_array().context(format!(
        "Pack {pack}, setting <settings>, option <schema>: expected array"
    ))?;
    ensure!(
        settings.len() <= 8,
        "Pack {pack}, setting <settings>, option <schema>: too many settings"
    );
    let mut ids = BTreeSet::new();
    let mut prefix = raw.clone();
    prefix.as_object_mut().unwrap().remove("settings");
    for raw_setting in settings {
        let id = raw_setting["id"].as_str().unwrap_or("<unknown>");
        let context = || format!("Pack {pack}, setting {id}, option <schema>");
        // Parse each option separately so malformed option fields retain their identity.
        let options = raw_setting["options"].as_array().with_context(context)?;
        for raw_option in options {
            let option = raw_option["id"].as_str().unwrap_or("<unknown>");
            serde_json::from_value::<OptionSpec>(raw_option.clone())
                .with_context(|| format!("Pack {pack}, setting {id}, option {option}"))?;
        }
        let setting: Setting = serde_json::from_value(raw_setting.clone()).with_context(context)?;
        ensure!(
            setting_id(id) && ids.insert(id) && label(&setting.label),
            "{}: invalid or duplicate setting",
            context()
        );
        ensure!(
            !setting.options.is_empty() && setting.options.len() <= 16,
            "{}: invalid option count",
            context()
        );
        let mut option_ids = BTreeSet::new();
        for option in &setting.options {
            let ctx = format!("Pack {pack}, setting {id}, option {}", option.id);
            ensure!(
                valid_id(&option.id) && option_ids.insert(&option.id) && label(&option.label),
                "{ctx}: invalid or duplicate option"
            );
            let patch = option
                .patch
                .as_object()
                .with_context(|| format!("{ctx}: patch must be an object"))?;
            ensure!(
                !["settings", "id", "revision", "schema_version"]
                    .iter()
                    .any(|key| patch.contains_key(*key)),
                "{ctx}: patch changes pack identity or schema"
            );
            let mut candidate = prefix.clone();
            merge_patch(&mut candidate, &option.patch);
            Pack::parse_descriptor(&serde_json::to_vec(&candidate)?)
                .with_context(|| format!("{ctx}: invalid descriptor patch"))?;
        }
        let default = setting
            .options
            .iter()
            .find(|o| o.id == setting.default)
            .with_context(|| {
                format!(
                    "Pack {pack}, setting {id}, option {}: unknown default",
                    setting.default
                )
            })?;
        merge_patch(&mut prefix, &default.patch);
    }
    Ok(())
}

pub(super) fn resolve(bytes: &[u8], selected: &Values) -> Result<(Pack, Values, String)> {
    let base = Pack::parse(bytes)?;
    let mut raw: Value = serde_json::from_slice(bytes)?;
    raw.as_object_mut().unwrap().remove("settings");
    let mut values = Values::new();
    for setting in &base.settings {
        let option = setting
            .options
            .iter()
            .find(|o| selected.get(&setting.id) == Some(&o.id))
            .unwrap_or_else(|| {
                setting
                    .options
                    .iter()
                    .find(|o| o.id == setting.default)
                    .unwrap()
            });
        merge_patch(&mut raw, &option.patch);
        // Selected combinations are validated in declaration order as well.
        Pack::parse_descriptor(&serde_json::to_vec(&raw)?).with_context(|| {
            format!(
                "Pack {}, setting {}, option {}: invalid resolved descriptor",
                base.id, setting.id, option.id
            )
        })?;
        values.insert(setting.id.clone(), option.id.clone());
    }
    let mut pack = Pack::parse_descriptor(&serde_json::to_vec(&raw)?)?;
    pack.settings = base.settings.clone();
    let base_digest = super::runtime::digest(bytes);
    let digest = if base.settings.is_empty() {
        base_digest
    } else {
        let ordered: Vec<_> = base
            .settings
            .iter()
            .map(|s| (&s.id, &values[&s.id]))
            .collect();
        super::runtime::digest(&serde_json::to_vec(&(base_digest, ordered))?)
    };
    Ok((pack, values, digest))
}

pub fn migrate(id: &str, selected: &Values) -> (String, Values) {
    let mut selected = selected.clone();
    if let Some(base) = id.strip_suffix("-tall") {
        selected.insert("cover_shape".into(), "tall".into());
        (base.into(), selected)
    } else {
        (id.into(), selected)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Boundary, ThemeRuntime, runtime::ThemeSnapshot};
    use super::*;
    use serde_json::json;

    fn descriptor() -> Value {
        let mut raw: Value =
            serde_json::from_slice(Pack::builtin_bytes("compact").unwrap()).unwrap();
        raw["settings"] = json!([
            {"id":"first_option","label":"First","default":"original","options":[
                {"id":"original","label":"Original","patch":{}},
                {"id":"changed","label":"Changed","patch":{"name":"First name"}}]},
            {"id":"second","label":"Second","default":"original","options":[
                {"id":"original","label":"Original","patch":{}},
                {"id":"changed","label":"Changed","patch":{"name":"Second name"}}]}
        ]);
        raw
    }
    #[test]
    fn schema_optional_and_ordered_options_digest_and_fallback() {
        let plain = Pack::builtin_bytes("compact").unwrap();
        assert_eq!(
            resolve(plain, &Values::new()).unwrap().2,
            super::super::runtime::digest(plain)
        );
        let bytes = serde_json::to_vec(&descriptor()).unwrap();
        let (base, values, hash) = resolve(
            &bytes,
            &Values::from([
                ("first_option".into(), "missing".into()),
                ("unknown".into(), "ignored".into()),
            ]),
        )
        .unwrap();
        assert_eq!(base.name, "Compact");
        assert_eq!(
            values,
            Values::from([
                ("first_option".into(), "original".into()),
                ("second".into(), "original".into())
            ])
        );
        let (first, _, first_hash) = resolve(
            &bytes,
            &Values::from([("first_option".into(), "changed".into())]),
        )
        .unwrap();
        assert_eq!(first.name, "First name");
        let (last, _, last_hash) = resolve(
            &bytes,
            &Values::from([
                ("first_option".into(), "changed".into()),
                ("second".into(), "changed".into()),
            ]),
        )
        .unwrap();
        assert_eq!(last.name, "Second name");
        assert_ne!(hash, first_hash);
        assert_ne!(first_hash, last_hash);
        let mut equal_pixels = descriptor();
        equal_pixels["settings"][0]["options"][1]["patch"] = json!({});
        let bytes = serde_json::to_vec(&equal_pixels).unwrap();
        assert_ne!(
            resolve(&bytes, &Values::new()).unwrap().2,
            resolve(
                &bytes,
                &Values::from([("first_option".into(), "changed".into())])
            )
            .unwrap()
            .2
        );
    }
    #[test]
    fn merge_patch_replaces_arrays_deletes_members_and_adds_objects() {
        let mut target = json!({"a":{"keep":1,"remove":2},"array":[1,2],"scalar":3});
        merge_patch(
            &mut target,
            &json!({"a":{"remove":null,"new":4},"array":[9],"scalar":{"x":1},"missing":null}),
        );
        assert_eq!(
            target,
            json!({"a":{"keep":1,"new":4},"array":[9],"scalar":{"x":1}})
        );
        merge_patch(&mut target, &json!([1]));
        assert_eq!(target, json!([1]));
    }
    #[test]
    fn schema_validation_names_pack_setting_and_option() {
        for (field, bad) in [
            ("id", json!("Bad option")),
            ("label", json!("")),
            ("patch", json!([])),
            ("patch", json!({"canvas":{"width":1}})),
            ("patch", json!({"id":"other"})),
            ("patch", json!({"settings":[]})),
            (
                "patch",
                json!({"assets":{"evil":{"path":"../escape","bytes":1,"width":1,"height":1,"sha256":"0".repeat(64),"required":true}}}),
            ),
        ] {
            let mut raw = descriptor();
            raw["settings"][0]["options"][1][field] = bad;
            let error = format!(
                "{:#}",
                Pack::parse(&serde_json::to_vec(&raw).unwrap()).unwrap_err()
            );
            assert!(
                error.contains("Pack compact, setting first_option, option"),
                "{error}"
            );
        }
        for mutation in 0..7 {
            let mut raw = descriptor();
            match mutation {
                0 => raw["settings"][0]["default"] = json!("missing"),
                1 => raw["settings"][1]["id"] = json!("first_option"),
                2 => raw["settings"][0]["options"][1]["id"] = json!("original"),
                3 => raw["settings"][0]["options"] = json!([]),
                4 => raw["settings"][0]["label"] = json!(""),
                5 => raw["settings"][0]["id"] = json!("bad setting"),
                _ => raw["settings"][0]["options"][0]["extra"] = json!(true),
            }
            let error = format!(
                "{:#}",
                Pack::parse(&serde_json::to_vec(&raw).unwrap()).unwrap_err()
            );
            assert!(
                error.contains("Pack compact, setting") && error.contains("option"),
                "{error}"
            );
        }
    }
    #[test]
    fn selected_combination_validated_in_declaration_order() {
        let mut raw = descriptor();
        raw["settings"][0]["options"][1]["patch"] =
            json!({"tokens":{"typography":{"row":{"size":24,"line_height":24}}}});
        raw["settings"][1]["options"][1]["patch"] =
            json!({"tokens":{"typography":{"row":{"line_height":20}}}});
        let bytes = serde_json::to_vec(&raw).unwrap();
        Pack::parse(&bytes).unwrap();
        assert!(
            resolve(
                &bytes,
                &Values::from([
                    ("first_option".into(), "changed".into()),
                    ("second".into(), "changed".into())
                ])
            )
            .is_err()
        );
    }
    fn roots() -> (std::path::PathBuf, std::path::PathBuf) {
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        (repo.join("themes"), repo.join("static/.allium/fonts"))
    }
    #[test]
    fn runtime_migrates_tall_missing_unknown_and_persists_canonical_values() {
        let (themes, fonts) = roots();
        let root = std::env::temp_dir()
            .canonicalize()
            .unwrap()
            .join(format!("allium-settings-migration-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        for (id, settings, shape, color) in [
            ("compact-plus-tall", None, "tall", "orange"),
            ("compact-plus", None, "square", "orange"),
            (
                "compact-plus",
                Some(json!({"cover_shape":"bogus","color_scheme":"black","extra":"x"})),
                "square",
                "black",
            ),
        ] {
            let mut record = json!({"schema_version":1,"id":id,"revision":"ink-port-v1","digest":"0".repeat(64),"generation":1});
            // Use the pack's actual revision, retaining the old descriptor digest for migration.
            let pack = ThemeSnapshot::load(&themes.join("compact-plus")).unwrap();
            record["revision"] = pack.0.pack.revision.clone().into();
            if let Some(settings) = settings {
                record["settings"] = settings;
            }
            std::fs::write(
                root.join("ui-theme.json"),
                serde_json::to_vec(&record).unwrap(),
            )
            .unwrap();
            let mut runtime = ThemeRuntime::bind(&root, &themes, &fonts).unwrap();
            let active = runtime.activate(Boundary::Launcher);
            assert!(active.status.diagnostic.is_none(), "{:?}", active.status);
            let handle = active.snapshot.unwrap();
            assert_eq!(handle.pack().id, "compact-plus");
            assert_eq!(handle.settings()["cover_shape"], shape);
            assert_eq!(handle.settings()["color_scheme"], color);
            let saved = super::super::selection::SelectionStore::bind(&root)
                .unwrap()
                .read()
                .unwrap()
                .unwrap();
            assert_eq!(saved.id, "compact-plus");
            assert_eq!(&saved.settings, handle.settings());
            assert_eq!(saved.digest, handle.digest());
            assert_eq!(saved.generation, 2);
            let menu = ThemeRuntime::bind(&root, &themes, &fonts)
                .unwrap()
                .activate_menu("en")
                .snapshot
                .unwrap();
            assert_eq!(menu.digest(), handle.digest());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn compact_schemes_both_shapes_prepare_and_change_digests_and_assets() {
        let (themes, _) = roots();
        let mut hashes = BTreeSet::new();
        for shape in ["square", "tall"] {
            for scheme in ["orange", "big-picture", "white", "black"] {
                let snapshot = ThemeSnapshot::load_with_settings(
                    &themes.join("compact-plus"),
                    &Values::from([
                        ("cover_shape".into(), shape.into()),
                        ("color_scheme".into(), scheme.into()),
                    ]),
                )
                .unwrap();
                assert!(hashes.insert(snapshot.0.digest.clone()));
                assert_eq!(
                    snapshot.0.pack.alignment.as_ref().unwrap().cover_shape,
                    shape
                );
                for asset in snapshot.0.pack.assets.values() {
                    assert_eq!(
                        asset.path.contains(&format!("-{scheme}.png")),
                        scheme != "orange"
                    );
                }
            }
        }
    }
}
