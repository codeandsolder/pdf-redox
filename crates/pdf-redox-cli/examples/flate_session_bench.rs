use clap::Parser;
use pdf_redox::{Config, FlateGpuConfig, OptimizationSession, SourcePdf};
use serde_json::json;
use std::{
    error::Error,
    fs,
    num::{NonZeroU32, NonZeroUsize},
    path::{Path, PathBuf},
    time::Instant,
};

#[derive(Parser)]
struct Args {
    #[arg(required = true)]
    inputs: Vec<PathBuf>,
    #[arg(long, default_value = "4")]
    workers: NonZeroUsize,
    #[arg(long, default_value = "512")]
    budget: NonZeroU32,
    #[arg(long, default_value_t = 1024 * 1024)]
    min_stream_bytes: usize,
    #[arg(long)]
    window_bytes: Option<NonZeroUsize>,
    #[arg(long)]
    adapter: Option<String>,
    #[arg(long, default_value = "2")]
    rounds: NonZeroUsize,
    #[arg(long)]
    cold: bool,
    #[arg(long)]
    output_dir: Option<PathBuf>,
}

fn measure(
    session: &OptimizationSession,
    path: &Path,
    input: &[u8],
    mode: &str,
    round: usize,
    output_dir: Option<&Path>,
) -> Result<(), Box<dyn Error>> {
    let start = Instant::now();
    let (output, report) = session.optimize(input)?;
    let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
    let output_bytes = output.len();
    if round == 0
        && let Some(dir) = output_dir
    {
        let name = path.file_stem().ok_or("input has no file stem")?;
        let mut name = name.to_os_string();
        name.push(format!(".{mode}.pdf"));
        fs::write(dir.join(name), &output)?;
    }
    let page_count = SourcePdf::from_bytes(output)?.page_count();
    if page_count != report.before.page_count || output_bytes != report.after_bytes {
        return Err("rewritten PDF page count or size does not match its report".into());
    }
    println!(
        "{}",
        json!({
            "input": path,
            "mode": mode,
            "round": round,
            "elapsed_ms": elapsed_ms,
            "input_bytes": input.len(),
            "output_bytes": output_bytes,
            "pages": page_count,
            "flate_selected": report.flate_streams_selected_for_recompression,
            "flate_batch": report.flate_batch,
            "stages_ms": report.stage_timings_ms,
        })
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    if let Some(dir) = &args.output_dir {
        fs::create_dir_all(dir)?;
    }
    let cpu_config = Config {
        flate_workers: args.workers,
        ..Config::optimize_only()
    };
    let gpu_config = Config {
        flate_gpu: Some(FlateGpuConfig {
            adapter: args.adapter,
            search_budget: args.budget,
            min_stream_bytes: args.min_stream_bytes,
            match_window_bytes: args.window_bytes,
        }),
        ..cpu_config.clone()
    };
    let cpu = OptimizationSession::new(cpu_config)?;
    let gpu = OptimizationSession::new(gpu_config.clone())?;
    for round in 0..args.rounds.get() {
        for (index, path) in args.inputs.iter().enumerate() {
            let input = fs::read(path)?;
            let mut modes = [(&cpu, "cpu"), (&gpu, "gpu-session")];
            if (index + round) % 2 != 0 {
                modes.reverse();
            }
            for (session, mode) in modes {
                measure(
                    session,
                    path,
                    &input,
                    mode,
                    round,
                    args.output_dir.as_deref(),
                )?;
            }
            if args.cold && round == 0 {
                measure(
                    &OptimizationSession::new(gpu_config.clone())?,
                    path,
                    &input,
                    "gpu-cold",
                    round,
                    args.output_dir.as_deref(),
                )?;
            }
        }
    }
    Ok(())
}
