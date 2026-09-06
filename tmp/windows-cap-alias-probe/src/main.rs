#[cfg(not(windows))]
compile_error!("this probe must run on Windows");

use cap_fs_ext::{DirExt, MetadataExt};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

type Identity = (u64, u64);

fn identity(metadata: &impl MetadataExt) -> Identity {
    (metadata.dev(), metadata.ino())
}

fn report(root: &Dir, name: &str, baseline_identity: Identity) {
    println!("name={name:?}");
    match root.symlink_metadata(name) {
        Ok(metadata) => println!(
            "  symlink_metadata=ok is_dir={} identity={:?} same_as_backups={}",
            metadata.is_dir(),
            identity(&metadata),
            identity(&metadata) == baseline_identity
        ),
        Err(error) => println!(
            "  symlink_metadata=err kind={:?} raw_os_error={:?}",
            error.kind(),
            error.raw_os_error()
        ),
    }

    match root.open_dir_nofollow(name) {
        Ok(opened) => {
            let marker = opened.read_to_string("probe-marker.txt");
            let opened_identity = opened
                .into_std_file()
                .metadata()
                .map(|metadata| identity(&metadata));
            println!(
                "  open_dir_nofollow=ok identity={opened_identity:?} same_as_backups={} marker={marker:?}",
                opened_identity
                    .as_ref()
                    .is_ok_and(|identity| *identity == baseline_identity)
            );
        }
        Err(error) => println!(
            "  open_dir_nofollow=err kind={:?} raw_os_error={:?}",
            error.kind(),
            error.raw_os_error()
        ),
    }
}

fn main() -> io::Result<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before Unix epoch")
        .as_nanos();
    let fixture = std::env::temp_dir().join(format!(
        "zeroclaw-cap-alias-probe-{}-{nonce}",
        std::process::id()
    ));
    std::fs::create_dir(&fixture)?;

    let root = Dir::open_ambient_dir(&fixture, ambient_authority())?;
    root.create_dir("backups")?;
    root.write("backups/probe-marker.txt", b"zeroclaw-windows-alias-probe")?;
    let baseline = root.open_dir_nofollow("backups")?;
    let baseline_identity = identity(&baseline.into_std_file().metadata()?);

    println!("fixture={}", fixture.display());
    println!("baseline_identity={baseline_identity:?}");
    for name in ["backups", "backups.", "backups "] {
        report(&root, name, baseline_identity);
    }
    Ok(())
}
