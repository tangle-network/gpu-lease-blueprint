//! Protected atomic JSON writes shared by local devices and cloud deployment intent.
use serde::Serialize;
use std::{fs::File, fs::OpenOptions, io::Write, path::Path};

pub(crate) fn write(path: &Path, value: &impl Serialize) -> Result<(), Box<dyn std::error::Error>> {
    let tmp = path.with_extension("pending");
    let mut options = OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    file.write_all(&serde_json::to_vec(value)?)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path)?;
    File::open(path.parent().ok_or("journal has no parent")?)?.sync_all()?;
    Ok(())
}
