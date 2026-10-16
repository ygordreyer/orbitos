#[test]
#[serial_test::serial(orbit_fixture)]
fn dl12_host_catalog_microbench() {
    if std::env::var_os("DL12_BENCH").is_none() {
        return;
    }
    let f = StartupFixture::new("dl12-catalog-bench");
    f.configured();
    let path = f.root.join(".allium/state/network-library/catalog.json");
    let mut catalog: Catalog = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for i in 1..43 {
        let mut game = f.game.clone();
        game.id = format!("bench-{i}");
        game.game_path = format!("Roms/PS/Bench {i}.chd");
        game.cache_path = game.game_path.clone();
        game.launch_path = game.game_path.clone();
        game.files[0].http_route = game.files[0].http_route.replace(&f.game.id, &game.id);
        catalog.games.push(game);
    }
    std::fs::write(path, serde_json::to_vec(&catalog).unwrap()).unwrap();
    initialize(f.root.clone()).unwrap();
    let target = f.root.join(&catalog.games[42].game_path);
    let mut samples = Vec::new();
    with((), |r| {
        for _ in 0..11 {
            let start = std::time::Instant::now();
            for _ in 0..1000 {
                std::hint::black_box(r.descriptor(std::hint::black_box(&target)).unwrap());
            }
            samples.push(start.elapsed().as_nanos() / 1000);
        }
    });
    samples.sort_unstable();
    println!(
        "DL12_HOST_MICRO {{\"operation\":\"catalog_lookup\",\"median_ns\":{},\"games\":43,\"release\":{}}}",
        samples[5],
        !cfg!(debug_assertions)
    );
}
