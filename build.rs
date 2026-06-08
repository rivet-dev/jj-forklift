use vergen_git2::{Build, Cargo, Emitter, Git2, Rustc};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let build = Build::all_build();
    let cargo = Cargo::builder()
        .debug(true)
        .features(true)
        .opt_level(true)
        .target_triple(true)
        .build();
    let git = Git2::builder()
        .branch(true)
        .commit_count(true)
        .commit_date(true)
        .commit_timestamp(true)
        .describe(true, true, None)
        .dirty(true)
        .sha(false)
        .build();
    let rustc = Rustc::all_rustc();

    Emitter::default()
        .add_instructions(&build)?
        .add_instructions(&cargo)?
        .add_instructions(&git)?
        .add_instructions(&rustc)?
        .emit()?;

    Ok(())
}
