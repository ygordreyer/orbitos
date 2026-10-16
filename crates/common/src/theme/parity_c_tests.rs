use super::*;
fn audit_value(v: &serde_json::Value, key: &str) -> String {
    v[key].as_str().unwrap_or("").chars().take(512).collect()
}
thread_local!{static AUDIT_ART_CACHE:std::cell::RefCell<std::collections::BTreeMap<String,Arc<tiny_skia::Pixmap>>>=const{std::cell::RefCell::new(std::collections::BTreeMap::new())};}
fn audit_art(root: &Path, assets: &serde_json::Value, src: &str, w:u32,h:u32) -> Option<Arc<tiny_skia::Pixmap>> {
    let key=format!("{src}:{w}:{h}");if let Some(found)=AUDIT_ART_CACHE.with(|cache|cache.borrow().get(&key).cloned()){return Some(found);}
    let path=assets[src].as_str()?;
    let source=tiny_skia::Pixmap::load_png(root.join(path)).unwrap();
    let ratio=(w as f64/source.width() as f64).min(h as f64/source.height() as f64).min(1.0);
    let (w,h)=((source.width() as f64*ratio).round() as u32,(source.height() as f64*ratio).round() as u32);
    let image=Arc::new(crate::theme::runtime::fit_image(&source,w,h,crate::theme::pack::Fit::Contain).unwrap());AUDIT_ART_CACHE.with(|cache|{cache.borrow_mut().insert(key,image.clone());});Some(image)
}
fn audit_input(record: &serde_json::Value, ready: &ThemeHandle, root: &Path, assets: &serde_json::Value) -> FrameInputs {
    let m=&record["model"];
    let screen:ScreenId=serde_json::from_value(record["screen"].clone()).unwrap();
    let mut f=FrameInputs::new(screen);
    for name in ["title","subtitle","breadcrumb","selected_title","selected_metadata","selected_blurb","selected_source","empty_title","empty_hint","message","keyboard_value","guide_text"] {
        let binding:Binding=serde_json::from_value(name.into()).unwrap();f.text.insert(binding,audit_value(m,name));
    }
    if matches!(screen, ScreenId::Search | ScreenId::Keyboard) && f.text.get(&Binding::KeyboardValue).is_some_and(|value| value == "Search games") {
        f.text.insert(Binding::KeyboardValue, String::new());
    }
    f.text.insert(Binding::ThemeName,ready.pack().name.clone());
    if let Some(p)=m["pagination"].as_object(){let page=p["page"].as_u64().unwrap_or(1);let total=p["total"].as_u64().unwrap_or(1);f.text.insert(Binding::Pagination,format!("{page} / {total}"));}
    for (name,target) in [("selected_metadata_runs",&mut f.metadata),("selected_facts",&mut f.facts)] {
        if let Some(rows)=m[name].as_array(){for row in rows{target.push((audit_value(row,"label"),audit_value(row,"value")));}}
    }
    for name in ["entries","settings_rows","discs","storage_entries","guides","actions"] {
        let binding:Binding=serde_json::from_value(name.into()).unwrap();
        let c=ready.pack().screens[&screen].components.iter().find(|c|c.binding==Some(binding));
        let count=c.and_then(|c|c.visible_count).unwrap_or(16) as usize;
        let mut rows=Vec::new();
        for (i,row) in m[name].as_array().into_iter().flatten().filter(|r|r["empty"]!=true).take(count).enumerate(){
            let id=row["actionId"].as_str().or(row["gameId"].as_str()).or(row["id"].as_str()).map(str::to_owned).unwrap_or_else(||format!("{name}-{i}"));
            if let Some(source)=row["image"].as_str().filter(|_|c.is_some()){if let Some(image)=audit_art(root,assets,source,160,180){f.art.entries.insert(id.clone(),image);}}
            let val=if row["value"].as_str().is_some_and(|s|!s.is_empty()){audit_value(row,"value")}else if c.and_then(|c|c.paint.as_ref()).is_some_and(|p|p.subtitle.is_some()){audit_value(row,"subtitle")}else{String::new()};
            rows.push(Row{id,label:audit_value(row,"title"),value:val,enabled:row["disabled"]!=true});
        }
        f.rows.insert(binding,rows);
    }
    for (name,group) in [("primary",NavGroup::Primary),("library_actions",NavGroup::LibraryActions)] {
        let rows=m["navigation_groups"][name].as_array().unwrap().iter().map(|row|Row{id:audit_value(row,"id"),label:audit_value(row,"title"),value:String::new(),enabled:true}).collect();f.navigation.insert(group,rows);
    }
    let nav=m["activeNav"].as_u64().unwrap_or(1) as usize;
    if let Some(row)=f.navigation[&NavGroup::Primary].get(nav){f.selected.insert(Binding::Navigation,row.id.clone());}
    let focused=m["focused"].as_u64().unwrap_or(0) as usize;
    let focus_binding=match screen{ScreenId::Library|ScreenId::SearchResults|ScreenId::Apps|ScreenId::ContextMenu|ScreenId::IngameMenu=>Some(Binding::Entries),ScreenId::SettingsRoot|ScreenId::SettingsPage=>Some(Binding::SettingsRows),ScreenId::DiscPicker=>Some(Binding::Discs),ScreenId::GuideSelector=>Some(Binding::Guides),ScreenId::GameDetail|ScreenId::Toast|ScreenId::NetworkDialog=>Some(Binding::Actions),_=>None};
    if let Some(binding)=focus_binding {if let Some(c)=ready.pack().screens[&screen].components.iter().find(|c|c.binding==Some(binding)){if let Some(row)=f.rows[&binding].get(focused.min(f.rows[&binding].len().saturating_sub(1))){if row.enabled {f.focus=Focus{component:c.id.clone(),item:Some(row.id.clone())};}}}}
    for hint in m["hints"].as_array().unwrap(){let button=audit_value(hint,"button");f.hints.push(Hint{button,label:audit_value(hint,"label")});}
    f.status.clock=Some(audit_value(&m["status"],"clock"));f.status.battery=Some(m["status"]["battery"].as_u64().unwrap_or(82) as u8);f.status.network=Some(audit_value(&m["status"],"network"));f.status.charging=m["status"]["charging"]==true;
    f.progress=m["progress"].as_f64().map(|p|(p/100.0)as f32);
    f.keyboard=(0..45).map(|_|String::new()).collect();
    for (row,letters) in ["QWERTYUIOP","ASDFGHJKL","ZXCVBNM"].iter().enumerate(){for (col,ch) in letters.chars().enumerate(){f.keyboard[row*11+col]=ch.to_string();}}
    f.keyboard[33]="⌫".into();f.keyboard[34]="Clear".into();f.keyboard[35]="Results".into();f.keyboard[44]="Space".into();
    f.keyboard_cursor=(0,0);
    f.art.selected_cover=audit_art(root,assets,m["selected_cover"].as_str().unwrap_or(""),320,320);
    f.art.selected_hero=audit_art(root,assets,m["selected_hero"].as_str().unwrap_or(""),640,360);
    f.art.next_cover=audit_art(root,assets,m["next_cover"].as_str().unwrap_or(""),160,180);
    f
}

#[test]
fn parity_c_semantic_keyboard_query_hints_and_options() {
    let bank = FontBank::bind(&font_root()).unwrap();
    for base in ["orbit-hero", "orbit-row", "big-picture", "compact-plus"] {
        for shape in ["square", "tall"] {
            for accent in if base == "compact-plus" { vec!["orange", "big-picture", "white", "black"] } else { vec!["default"] } {
                let mut settings = std::collections::BTreeMap::from([("cover_shape".into(), shape.into())]);
                if accent != "default" { settings.insert("color_scheme".into(), accent.into()); }
                let ready = ThemeHandle::prepare(ThemeSnapshot::load_with_settings(&ink_pack_root().join(base), &settings).unwrap(), &bank, 1).unwrap();
                let mut input = frame(ScreenId::Search);
                input.keyboard = vec![String::new(); 45];
                for (row, letters) in ["QWERTYUIOP", "ASDFGHJKL", "ZXCVBNM"].iter().enumerate() {
                    for (col, key) in letters.chars().enumerate() { input.keyboard[row * 11 + col] = key.to_string(); }
                }
                input.keyboard[33] = "Backspace".into(); input.keyboard[34] = "Clear".into(); input.keyboard[35] = "Results".into(); input.keyboard[44] = "Space".into();
                let rendered = PreparedFrame::prepare(ready.clone(), &input).unwrap();
                let keys: Vec<_> = rendered.targets.iter().filter(|t| t.item.starts_with("key-")).collect();
                assert_eq!(keys.len(), 30);
                for (row, count) in [10, 9, 7, 3, 1].iter().enumerate() {
                    assert_eq!(keys.iter().filter(|t| t.item.starts_with(&format!("key-{row}-"))).count(), *count);
                }
                let command = keys.iter().find(|t| t.item == "key-3-2").unwrap();
                let letter = keys.iter().find(|t| t.item == "key-0-0").unwrap();
                assert!(command.rect.2 > letter.rect.2 * 2);
                let query = ready.pack().screens[&ScreenId::Search].components.iter().find(|c| c.binding == Some(Binding::KeyboardValue)).unwrap();
                input.text.insert(Binding::KeyboardValue, String::new());
                let placeholder = PreparedFrame::prepare(ready.clone(), &input).unwrap();
                input.focus.component = query.id.clone();
                let focused = PreparedFrame::prepare(ready.clone(), &input).unwrap();
                assert_ne!(placeholder.pixels().data(), focused.pixels().data());
                let retained = PreparedFrame::prepare_reusing(ready.clone(), &input, Some(placeholder)).unwrap();
                assert_eq!(retained.pixels().data(), focused.pixels().data());
                input.focus = Focus::default();
                let unfocused = PreparedFrame::prepare_reusing(ready.clone(), &input, Some(retained)).unwrap();
                let fresh = PreparedFrame::prepare(ready.clone(), &input).unwrap();
                assert_eq!(unfocused.pixels().data(), fresh.pixels().data());
                input.focus.component = query.id.clone();
                input.hints = vec![Hint { button: "A".into(), label: "This must be overridden".into() }];
                let overridden = PreparedFrame::prepare(ready.clone(), &input).unwrap();
                assert_eq!(focused.pixels().data(), overridden.pixels().data());
                input.suppress_hints = true;
                let hidden = PreparedFrame::prepare_reusing(ready.clone(), &input, Some(overridden)).unwrap();
                let fresh_hidden = PreparedFrame::prepare(ready.clone(), &input).unwrap();
                assert_eq!(hidden.pixels().data(), fresh_hidden.pixels().data());
                assert_ne!(hidden.pixels().data(), focused.pixels().data());
                input.suppress_hints = false;
                let restored = PreparedFrame::prepare_reusing(ready.clone(), &input, Some(hidden)).unwrap();
                assert_eq!(restored.pixels().data(), focused.pixels().data());
                let mut invalid = ready.pack().screens[&ScreenId::Search].components.iter().find(|c| c.binding == Some(Binding::Hints)).unwrap().paint.clone().unwrap();
                invalid.hint_items.as_mut().unwrap()[0].button = Some("Untrusted".into());
                assert!(invalid.validate().is_err());
                for button in ["Up/Down", "Left/Right"] { input.hints = vec![Hint { button: button.into(), label: "Axis".into() }]; input.validate(&ready).unwrap(); }
            }
        }
    }
}

#[test]
fn parity_c_capture_equal_web_models_all_options() {
    let Ok(output) = std::env::var("PARITY_C_FRAMES") else { return; };
    let root = PathBuf::from(std::env::var("PARITY_C_REFERENCE").expect("PARITY_C_REFERENCE is required for capture"));
    let data: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("vm-models.json")).unwrap()).unwrap();
    let assets: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("model-assets.json")).unwrap()).unwrap();
    let bank = FontBank::bind(&font_root()).unwrap();
    let mut manifest = Vec::new();
    for record in data["records"].as_array().unwrap() {
        let id = record["pack"].as_str().unwrap();
        let base = id.trim_end_matches("-tall");
        let shape = if id.ends_with("-tall") { "tall" } else { "square" };
        for accent in if base == "compact-plus" { vec!["orange", "big-picture", "white", "black"] } else { vec!["default"] } {
            let mut settings = std::collections::BTreeMap::from([("cover_shape".into(), shape.into())]);
            if accent != "default" { settings.insert("color_scheme".into(), accent.into()); }
            let ready = ThemeHandle::prepare(ThemeSnapshot::load_with_settings(&ink_pack_root().join(base), &settings).unwrap(), &bank, 500).unwrap();
            let input = audit_input(record, &ready, &root, &assets);
            let rendered = PreparedFrame::prepare(ready.clone(), &input).unwrap();
            let mut pixels = rendered.pixels().clone();
            if input.screen.is_dialog() || input.screen.is_osd() {
                let library = data["records"].as_array().unwrap().iter().find(|r| r["pack"] == id && r["screen"] == "library").unwrap();
                let mut base = audit_input(library, &ready, &root, &assets); base.hints.clear(); base.suppress_hints = true;
                pixels = PreparedFrame::prepare(ready, &base).unwrap().pixels().clone();
                pixels.draw_pixmap(0, 0, rendered.pixels().as_ref(), &tiny_skia::PixmapPaint::default(), tiny_skia::Transform::identity(), None);
            }
            let name = record["screen"].as_str().unwrap();
            let dir = Path::new(&output).join(base).join(shape).join(accent);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(format!("{name}-native.png")); pixels.save_png(&path).unwrap();
            manifest.push(serde_json::json!({"theme":base,"shape":shape,"accent":accent,"screen":name,"path":path,"sha256":crate::theme::runtime::digest(pixels.data()),"historical_web":root.join("frames").join(id).join(format!("{name}-web-historical.png"))}));
        }
    }
    assert_eq!(manifest.len(), 252);
    std::fs::write(Path::new(&output).join("frames.json"), serde_json::to_string_pretty(&manifest).unwrap()).unwrap();
    println!("PARITY_C_FRAMES={}", manifest.len());
}
