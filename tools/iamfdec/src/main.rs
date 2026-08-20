//! Inspects a standalone .iamf bitstream, listing its OBUs.
//!
//! Will grow into a full decode-to-WAV tool (the Rust counterpart of
//! libiamf's iamfdec) as pipeline milestones land.

use std::process::ExitCode;

use iamf_obu::ObuIter;

fn main() -> ExitCode {
    let Some(path) = std::env::args().nth(1) else {
        eprintln!("usage: iamfdec <file.iamf>");
        return ExitCode::FAILURE;
    };
    let data = match std::fs::read(&path) {
        Ok(data) => data,
        Err(err) => {
            eprintln!("error: cannot read {path}: {err}");
            return ExitCode::FAILURE;
        }
    };

    let mut count = 0usize;
    for result in ObuIter::new(&data) {
        match result {
            Ok(obu) => {
                count += 1;
                let header = &obu.header;
                print!(
                    "{:>4}: {:?} payload={}B",
                    count,
                    header.obu_type,
                    obu.payload.len()
                );
                if header.redundant_copy {
                    print!(" redundant");
                }
                if header.num_samples_to_trim_at_start != 0
                    || header.num_samples_to_trim_at_end != 0
                {
                    print!(
                        " trim=[start {}, end {}]",
                        header.num_samples_to_trim_at_start, header.num_samples_to_trim_at_end
                    );
                }
                println!();
            }
            Err(err) => {
                eprintln!("error: {err}");
                return ExitCode::FAILURE;
            }
        }
    }
    println!("{count} OBUs");
    ExitCode::SUCCESS
}
