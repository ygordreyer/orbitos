use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct Counts {
    pub read_attempts: u64,
    pub decodes: u64,
    pub art_resizes: u64,
    pub fits: u64,
    pub frames: u64,
    pub ram_hits: u64,
    pub disk_hits: u64,
}

pub enum Op {
    Read,
    Decode,
    ArtResize,
    Fit,
    Frame,
    RamHit,
    DiskHit,
}

thread_local! {
    static COUNTS: Cell<Counts> = Cell::new(Counts::default());
}

pub fn hit(op: Op) {
    COUNTS.with(|cell| {
        let mut c = cell.get();
        match op {
            Op::Read => c.read_attempts += 1,
            Op::Decode => c.decodes += 1,
            Op::ArtResize => c.art_resizes += 1,
            Op::Fit => c.fits += 1,
            Op::Frame => c.frames += 1,
            Op::RamHit => c.ram_hits += 1,
            Op::DiskHit => c.disk_hits += 1,
        }
        cell.set(c);
    });
}

pub fn take() -> Counts {
    COUNTS.with(|cell| cell.replace(Counts::default()))
}
