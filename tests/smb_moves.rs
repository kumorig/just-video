//! Run against a disposable local Samba share whose backing directory is given
//! by JUST_VIDEO_SMB_MOVE_TEST_ROOT and URL by JUST_VIDEO_SMB_MOVE_TEST_URL.

use just_video::smb::{SmbSession, SmbUrl};
use std::path::PathBuf;

struct Fixture(PathBuf);

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
#[ignore = "requires a disposable local Samba share"]
fn smb_moves_preserve_contents_and_never_overwrite() {
    let root =
        PathBuf::from(std::env::var("JUST_VIDEO_SMB_MOVE_TEST_ROOT").expect("test share root"));
    let url: SmbUrl = std::env::var("JUST_VIDEO_SMB_MOVE_TEST_URL")
        .expect("test share URL")
        .parse()
        .unwrap();
    assert!(
        !url.share.is_empty() && url.path.is_empty(),
        "URL must name the share root"
    );
    let share = url.share.clone();
    let name = format!(
        "move-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    let local = root.join(&name);
    std::fs::create_dir(&local).unwrap();
    let _fixture = Fixture(local.clone());
    std::fs::create_dir_all(local.join("source/folder/nested")).unwrap();
    std::fs::create_dir(local.join("destination")).unwrap();
    std::fs::write(local.join("source/film å.mp4"), b"video bytes").unwrap();
    std::fs::write(
        local.join("source/folder/nested/subtitle.srt"),
        b"subtitle bytes",
    )
    .unwrap();
    std::fs::write(local.join("source/collision.mp4"), b"source bytes").unwrap();
    std::fs::write(
        local.join("destination/collision.mp4"),
        b"destination bytes",
    )
    .unwrap();

    let session = SmbSession::connect(
        url,
        std::env::var("JUST_VIDEO_SMB_MOVE_TEST_PASSWORD").unwrap_or_default(),
    )
    .unwrap();
    let destination = format!("{name}\\destination");
    session
        .move_in(&share, &format!("{name}\\source\\film å.mp4"), &destination)
        .unwrap();
    assert!(!local.join("source/film å.mp4").exists());
    assert_eq!(
        std::fs::read(local.join("destination/film å.mp4")).unwrap(),
        b"video bytes"
    );

    session
        .move_in(&share, &format!("{name}\\source\\folder"), &destination)
        .unwrap();
    assert!(!local.join("source/folder").exists());
    assert_eq!(
        std::fs::read(local.join("destination/folder/nested/subtitle.srt")).unwrap(),
        b"subtitle bytes"
    );
    session
        .move_in(&share, &format!("{name}\\destination\\folder"), &name)
        .unwrap();
    assert!(local.join("folder/nested/subtitle.srt").exists());

    assert!(
        session
            .move_in(
                &share,
                &format!("{name}\\source\\collision.mp4"),
                &destination
            )
            .is_err()
    );
    assert_eq!(
        std::fs::read(local.join("source/collision.mp4")).unwrap(),
        b"source bytes"
    );
    assert_eq!(
        std::fs::read(local.join("destination/collision.mp4")).unwrap(),
        b"destination bytes"
    );

    assert!(
        session
            .move_in(
                &share,
                &format!("{name}\\folder"),
                &format!("{name}\\folder\\nested")
            )
            .is_err()
    );
    assert!(
        session
            .move_in(
                &share,
                &format!("{name}\\folder"),
                &format!("{name}\\missing")
            )
            .is_err()
    );
    assert!(local.join("folder/nested/subtitle.srt").exists());
    // The same SET_INFO helper still supports renaming after the refactor.
    session
        .rename_in(
            &share,
            &format!("{name}\\destination\\film å.mp4"),
            "renamed.mp4",
        )
        .unwrap();
    assert_eq!(
        std::fs::read(local.join("destination/renamed.mp4")).unwrap(),
        b"video bytes"
    );
}
