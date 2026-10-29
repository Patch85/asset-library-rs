//! `asset-library bench <folder>`: how fast this machine thumbnails, analyses and embeds,
//! and how much memory that takes, to choose low-end defaults (threads, batch size, int8).

use std::path::Path;
use std::time::Instant;

use crate::analysis::color_profile;
use crate::clip::Clip;
use crate::scanner::iter_images;
use crate::thumbs::render;

pub struct Options {
    pub limit: usize,
    pub batch: usize,
    pub threads: usize,
}

pub fn run(dir: &Path, model_dir: &Path, opts: &Options) -> anyhow::Result<()> {
    let files: Vec<_> = iter_images(dir).into_iter().take(opts.limit).collect();
    if files.is_empty() {
        anyhow::bail!("no images found in {}", dir.display());
    }
    println!("Asset Library {} benchmark", env!("CARGO_PKG_VERSION"));
    println!(
        "{} {}, {} logical CPUs, using {} threads, batch {}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism().map_or(0, |n| n.get()),
        opts.threads,
        opts.batch
    );
    println!("{} images from {}\n", files.len(), dir.display());

    let t = Instant::now();
    let pool = rayon::ThreadPoolBuilder::new().num_threads(opts.threads.max(1)).build()?;
    let thumbs: Vec<Vec<u8>> = pool.install(|| {
        use rayon::prelude::*;
        files.par_iter().filter_map(|(p, _)| render(p).ok()).collect()
    });
    report("thumbnails", thumbs.len(), t);

    let t = Instant::now();
    let images: Vec<_> = thumbs.iter().filter_map(|b| image::load_from_memory(b).ok()).collect();
    for im in &images {
        color_profile(im);
    }
    report("colours", images.len(), t);

    println!("\nloading CLIP (downloads about 600 MB into {} the first time)...", model_dir.display());
    let clip = Clip::new(model_dir.to_path_buf());
    let t = Instant::now();
    clip.embed_images(images[..1].to_vec(), opts.threads)?;
    println!("{:<12} {:>8.1} s", "model load", t.elapsed().as_secs_f64());
    let t = Instant::now();
    for chunk in images.chunks(opts.batch.max(1)) {
        clip.embed_images(chunk.to_vec(), opts.threads)?;
    }
    report("embeddings", images.len(), t);

    match peak_rss_bytes() {
        Some(b) => println!("\npeak memory  {:>8.0} MB", b as f64 / 1e6),
        None => println!("\npeak memory  unknown on this platform"),
    }
    Ok(())
}

fn report(what: &str, n: usize, since: Instant) {
    let secs = since.elapsed().as_secs_f64();
    println!("{what:<12} {:>8.1} images/s  ({n} in {secs:.1} s)", n as f64 / secs.max(1e-9));
}

/// Peak resident memory of this process so far.
fn peak_rss_bytes() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmHWM:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        Some(kb * 1024)
    }
    #[cfg(target_os = "macos")]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        // SAFETY: getrusage only writes into the struct we pass.
        (unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) } == 0).then_some(usage.ru_maxrss as u64)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        // SAFETY: the pseudo-handle needs no closing and the struct size is set.
        (unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) } != 0).then_some(c.PeakWorkingSetSize as u64)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        None
    }
}
