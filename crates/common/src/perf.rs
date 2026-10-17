use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
pub struct Counts {
    pub read_attempts: u64,
    pub media_io: u64,
    pub decodes: u64,
    pub art_resizes: u64,
    pub fits: u64,
    pub frames: u64,
    pub neighbor_matches: u64,
    pub ram_hits: u64,
    pub disk_hits: u64,
    pub page_history_scans: u64,
    pub page_history_marks: u64,
}

pub enum Op {
    Read,
    MediaIo,
    Decode,
    ArtResize,
    Fit,
    Frame,
    NeighborMatch,
    RamHit,
    DiskHit,
    PageHistoryScan,
    PageHistoryMark,
}

thread_local! {
    static COUNTS: Cell<Counts> = Cell::new(Counts::default());
}

pub fn hit(op: Op) {
    COUNTS.with(|cell| {
        let mut c = cell.get();
        match op {
            Op::MediaIo => c.media_io += 1,
            Op::Read => c.read_attempts += 1,
            Op::Decode => c.decodes += 1,
            Op::ArtResize => c.art_resizes += 1,
            Op::Fit => c.fits += 1,
            Op::Frame => c.frames += 1,
            Op::NeighborMatch => c.neighbor_matches += 1,
            Op::RamHit => c.ram_hits += 1,
            Op::DiskHit => c.disk_hits += 1,
            Op::PageHistoryScan => c.page_history_scans += 1,
            Op::PageHistoryMark => c.page_history_marks += 1,
        }
        cell.set(c);
    });
}

pub fn take() -> Counts {
    COUNTS.with(|cell| cell.replace(Counts::default()))
}
