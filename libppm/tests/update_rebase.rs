use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use libppm::update::{PreparedUpdate, RebaseOutcome};

fn run(program: &str, args: &[&Path]) {
    let out = Command::new(program).args(args).output().unwrap();
    assert!(
        out.status.success(),
        "{program}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn snapshot(source: &Path, destination: &Path) {
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            Path::new("-r"),
            source,
            destination,
        ],
    );
}

fn write(root: &Path, path: &str, data: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, data).unwrap();
}

fn package(path: &Path, version: &str, config: Option<&str>) {
    package_with_scriptlet(path, version, config, None);
}

fn package_with_scriptlet(
    path: &Path,
    version: &str,
    config: Option<&str>,
    scriptlet: Option<&str>,
) {
    let mut archive = tar::Builder::new(fs::File::create(path).unwrap());
    let info = format!("pkgname = pond-update-test\npkgver = {version}-1\npkgdesc = update test\narch = any\nsize = 16\nbackup = etc/example.conf\n");
    let mut members = vec![
        (".PKGINFO", info.as_str()),
        ("usr/share/example/version", version),
    ];
    if let Some(config) = config {
        members.push(("etc/example.conf", config));
    }
    if let Some(scriptlet) = scriptlet {
        members.push((".INSTALL", scriptlet));
    }
    for (name, data) in members {
        let mut header = tar::Header::new_gnu();
        header.set_size(data.len() as u64);
        header.set_mode(0o644);
        header.set_uid(0);
        header.set_gid(0);
        header.set_mtime(1_700_000_000);
        header.set_cksum();
        archive
            .append_data(&mut header, name, data.as_bytes())
            .unwrap();
    }
    archive.finish().unwrap();
}

// Install only the test shell/tools and their dynamic libraries into a fixture.
// Native scriptlets then execute inside pacman's chroot, never the host root.
fn fixture_shell(root: &Path) {
    use std::collections::BTreeSet;
    let mut paths = BTreeSet::new();
    for program in ["/usr/bin/bash", "/usr/bin/touch"] {
        paths.insert(PathBuf::from(program));
        let output = Command::new("ldd").arg(program).output().unwrap();
        assert!(output.status.success());
        for word in String::from_utf8(output.stdout).unwrap().split_whitespace() {
            if word.starts_with('/') {
                paths.insert(PathBuf::from(word));
            }
        }
    }
    for path in paths {
        let target = root.join(path.strip_prefix("/").unwrap());
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::copy(path, target).unwrap();
    }
    fs::create_dir_all(root.join("bin")).unwrap();
    std::os::unix::fs::symlink("../usr/bin/bash", root.join("bin/sh")).unwrap();
}

fn pacman(root: &Path, archive: &Path, conf: &Path) {
    let out = Command::new("pacman")
        .arg("--root")
        .arg(root)
        .arg("--dbpath")
        .arg(root.join("var/lib/pacman"))
        .arg("--logfile")
        .arg(root.join("var/log/pacman.log"))
        .arg("--config")
        .arg(conf)
        .args(["--noconfirm", "-U"])
        .arg(archive)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "pacman: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Compare the entire tree, not just selected payload paths. Native execution
/// time is intentionally excluded: INSTALLDATE, log text and directory mtimes
/// differ between two real invocations. Logs have their own append test below.
fn assert_tree_matches_pacman(actual: &Path, expected: &Path) {
    use std::{collections::BTreeMap, os::unix::fs::MetadataExt};
    #[derive(Debug, PartialEq, Eq)]
    struct Entry {
        mode: u32,
        uid: u32,
        gid: u32,
        data: Vec<u8>,
        target: Option<PathBuf>,
        mtime: Option<(i64, i64)>,
        attrs: Vec<(std::ffi::OsString, Vec<u8>)>,
    }
    fn tree(root: &Path, dir: &Path, result: &mut BTreeMap<PathBuf, Entry>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let m = fs::symlink_metadata(&path).unwrap();
            let relative = path.strip_prefix(root).unwrap().to_owned();
            let in_db = relative.starts_with("var/lib/pacman");
            let is_log = relative == Path::new("var/log/pacman.log");
            let mut data = if m.is_file() && !is_log {
                fs::read(&path).unwrap()
            } else {
                Vec::new()
            };
            if in_db && path.file_name().unwrap() == "desc" {
                let text = String::from_utf8(data).unwrap();
                let mut skip = false;
                let mut normalized = String::new();
                for line in text.lines() {
                    if skip {
                        skip = false;
                        continue;
                    }
                    normalized.push_str(line);
                    normalized.push('\n');
                    if line == "%INSTALLDATE%" {
                        skip = true;
                    }
                }
                data = normalized.into_bytes();
            }
            let mut attrs: Vec<_> = xattr::list(&path)
                .unwrap()
                .map(|k| {
                    let v = xattr::get(&path, &k).unwrap().unwrap();
                    (k, v)
                })
                .collect();
            attrs.sort();
            result.insert(
                relative,
                Entry {
                    mode: m.mode(),
                    uid: m.uid(),
                    gid: m.gid(),
                    data,
                    target: m.is_symlink().then(|| fs::read_link(&path).unwrap()),
                    mtime: (m.is_file() && !in_db && !is_log)
                        .then_some((m.mtime(), m.mtime_nsec())),
                    attrs,
                },
            );
            if m.is_dir() {
                tree(root, &path, result);
            }
        }
    }
    let mut a = BTreeMap::new();
    let mut b = BTreeMap::new();
    tree(actual, actual, &mut a);
    tree(expected, expected, &mut b);
    assert_eq!(
        a.keys().collect::<Vec<_>>(),
        b.keys().collect::<Vec<_>>(),
        "tree path set"
    );
    for (path, entry) in a {
        let expected = &b[&path];
        assert!(
            entry == *expected,
            "{}: mode={} owner={} content={} link={} mtime={} xattrs={}",
            path.display(),
            entry.mode == expected.mode,
            (entry.uid, entry.gid) == (expected.uid, expected.gid),
            entry.data == expected.data,
            entry.target == expected.target,
            entry.mtime == expected.mtime,
            entry.attrs == expected.attrs
        );
    }
}

/// Run native pacman only in a fresh user/mount/PID namespace, never against /.
fn isolated(test: &str) -> bool {
    if std::env::var_os("POND_UPDATE_TEST_WORKER").is_some() {
        return false;
    }
    let out = Command::new("unshare")
        .args(["--user", "--map-root-user", "--mount", "--pid", "--fork"])
        .arg(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--include-ignored"])
        .env("POND_UPDATE_TEST_WORKER", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "isolated test failed: {} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    eprint!("{}", String::from_utf8_lossy(&out.stderr));
    print!("{}", String::from_utf8_lossy(&out.stdout));
    true
}

struct Fixture {
    work: PathBuf,
    base: PathBuf,
    staged: PathBuf,
    live: PathBuf,
    archive: PathBuf,
    conf: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        Self::with_config(Some("new\n"))
    }

    fn with_config(next_config: Option<&str>) -> Self {
        Self::with_initial_edit(next_config, None)
    }

    fn with_initial_edit(next_config: Option<&str>, initial_edit: Option<&str>) -> Self {
        Self::with_background_files(next_config, initial_edit, 0)
    }

    fn with_background_files(
        next_config: Option<&str>,
        initial_edit: Option<&str>,
        files: usize,
    ) -> Self {
        // Btrfs scratch space, not /tmp (which may be tmpfs). Keep failed fixtures inspectable.
        let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/update-tests");
        fs::create_dir_all(&parent).unwrap();
        let work = tempfile::Builder::new()
            .prefix("rebase-")
            .tempdir_in(parent)
            .unwrap()
            .keep();
        eprintln!("fixture: {}", work.display());
        let live = work.join("live");
        run(
            "btrfs",
            &[Path::new("subvolume"), Path::new("create"), &live],
        );
        fs::create_dir_all(live.join("var/lib/pacman")).unwrap();
        fs::create_dir_all(live.join("var/log")).unwrap();
        let conf = work.join("pacman.conf");
        fs::write(
            &conf,
            "[options]\nArchitecture = x86_64\nSigLevel = Never\nLocalFileSigLevel = Never\n",
        )
        .unwrap();
        let first = work.join("first.pkg.tar");
        let archive = work.join("next.pkg.tar");
        package(&first, "1", Some("old\n"));
        package(&archive, "2", next_config);
        pacman(&live, &first, &conf);
        write(&live, "var/lib/user-app/state", "original\n");
        for index in 0..files {
            write(
                &live,
                &format!("var/lib/bulk-state/{index}"),
                &"baseline content".repeat(512),
            );
        }
        if let Some(edit) = initial_edit {
            write(&live, "etc/example.conf", edit);
        }
        let base = work.join("base");
        snapshot(&live, &base);
        let staging = work.join("staging");
        run(
            "btrfs",
            &[
                Path::new("subvolume"),
                Path::new("snapshot"),
                &live,
                &staging,
            ],
        );
        pacman(&staging, &archive, &conf);
        let staged = work.join("staged");
        snapshot(&staging, &staged);
        Self {
            work,
            base,
            staged,
            live,
            archive,
            conf,
        }
    }
}

#[test]
fn restored_backup_does_not_keep_a_stale_pacnew() {
    if isolated("restored_backup_does_not_keep_a_stale_pacnew") {
        return;
    }
    let f = Fixture::with_initial_edit(Some("new\n"), Some("old user edit\n"));
    assert!(f.staged.join("etc/example.conf.pacnew").exists());
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    write(&f.live, "etc/example.conf", "old\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let oracle = f.work.join("oracle");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &oracle,
        ],
    );
    pacman(&oracle, &f.archive, &f.conf);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("expected candidate")
    };
    assert_eq!(
        fs::read(candidate.path().join("etc/example.conf")).unwrap(),
        fs::read(oracle.join("etc/example.conf")).unwrap()
    );
    assert!(!candidate.path().join("etc/example.conf.pacnew").exists());
    assert_tree_matches_pacman(candidate.path(), &oracle);
}

#[test]
fn staged_database_reuse_needs_no_archives_and_allows_inactive_hooks() {
    if isolated("staged_database_reuse_needs_no_archives_and_allows_inactive_hooks") {
        return;
    }
    let f = Fixture::new();
    fs::create_dir_all(f.live.join("usr/share/libalpm/hooks")).unwrap();
    write(&f.live, "usr/share/libalpm/hooks/unrelated.hook",
        "[Trigger]\nOperation = Upgrade\nType = Package\nTarget = unrelated-package\n[Action]\nWhen = PostTransaction\nExec = /bin/false\n");
    let base = f.work.join("hooks-base");
    snapshot(&f.live, &base);
    let staging = f.work.join("hooks-staging");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &base,
            &staging,
        ],
    );
    pacman(&staging, &f.archive, &f.conf);
    let staged = f.work.join("hooks-staged");
    snapshot(&staging, &staged);
    let update = PreparedUpdate::prepare(
        &base,
        &staged,
        &[],
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    write(&f.live, "etc/example.conf", "late custom config\n");
    write(
        &f.live,
        "var/lib/user-app/state",
        "late application state\n",
    );
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let candidate = update
        .refresh(&latest, &f.work.join("candidate"), |_| {
            panic!("an inactive hook must not force native restaging")
        })
        .unwrap();
    assert!(!candidate.stats.native_restage);
    let oracle = f.work.join("oracle");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &oracle,
        ],
    );
    pacman(&oracle, &f.archive, &f.conf);
    assert_tree_matches_pacman(candidate.path(), &oracle);
}

#[test]
fn preparation_size_does_not_scale_with_unrelated_root_files() {
    if isolated("preparation_size_does_not_scale_with_unrelated_root_files") {
        return;
    }
    let f = Fixture::with_background_files(Some("new\n"), None, 1_000);
    let store = f.work.join("prepared");
    PreparedUpdate::prepare(&f.base, &f.staged, &[], Path::new("var/lib/pacman"), &store).unwrap();
    let size = fs::metadata(store.join("plan.json")).unwrap().len();
    assert!(
        size < 32 * 1024,
        "unrelated files inflated preparation to {size} bytes"
    );
    let update = PreparedUpdate::open(&store).unwrap();
    write(&f.live, "var/lib/user-app/state", "late state\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    assert!(matches!(
        update.rebase(&latest, &f.work.join("candidate")).unwrap(),
        RebaseOutcome::Candidate(_)
    ));
}

#[test]
fn missing_default_only_defers_when_the_late_change_needs_those_bytes() {
    if isolated("missing_default_only_defers_when_the_late_change_needs_those_bytes") {
        return;
    }
    let f = Fixture::with_initial_edit(Some("old\n"), Some("custom\n"));
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        &[],
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    write(&f.live, "var/lib/user-app/state", "unrelated late edit\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    assert!(matches!(
        update
            .rebase(&latest, &f.work.join("unrelated-candidate"))
            .unwrap(),
        RebaseOutcome::Candidate(_)
    ));
    fs::remove_file(f.live.join("etc/example.conf")).unwrap();
    let deleted = f.work.join("deleted");
    snapshot(&f.live, &deleted);
    assert!(matches!(
        update
            .rebase(&deleted, &f.work.join("missing-candidate"))
            .unwrap(),
        RebaseOutcome::NotReady(_)
    ));
    assert!(!f.work.join("missing-candidate").exists());
    let cached = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("cached-preparation"),
    )
    .unwrap();
    let RebaseOutcome::Candidate(candidate) = cached
        .rebase(&deleted, &f.work.join("recovered-candidate"))
        .unwrap()
    else {
        panic!("cached default must recover")
    };
    let oracle = f.work.join("oracle");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &deleted,
            &oracle,
        ],
    );
    pacman(&oracle, &f.archive, &f.conf);
    assert_tree_matches_pacman(candidate.path(), &oracle);
}

#[test]
fn systemd_update_hook_reuses_completed_output_for_unrelated_state_changes() {
    if isolated("systemd_update_hook_reuses_completed_output_for_unrelated_state_changes") {
        return;
    }
    let f = Fixture::new();
    fixture_shell(&f.live);
    let helper = "usr/share/libalpm/scripts/systemd-hook";
    fs::create_dir_all(f.live.join("usr/share/libalpm/scripts")).unwrap();
    fs::copy(
        "/usr/share/libalpm/scripts/systemd-hook",
        f.live.join(helper),
    )
    .unwrap();
    write(&f.live, "usr/share/libalpm/hooks/35-systemd-update.hook", "[Trigger]\nOperation = Upgrade\nType = Package\nTarget = pond-update-test\n[Action]\nWhen = PostTransaction\nExec = /usr/share/libalpm/scripts/systemd-hook update\n");
    let base = f.work.join("hook-base");
    snapshot(&f.live, &base);
    let staging = f.work.join("hook-staging");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &base,
            &staging,
        ],
    );
    pacman(&staging, &f.archive, &f.conf);
    assert!(fs::read_to_string(staging.join("var/log/pacman.log"))
        .unwrap()
        .contains("running '35-systemd-update.hook'"));
    let staged = f.work.join("hook-staged");
    snapshot(&staging, &staged);
    let update = PreparedUpdate::prepare(
        &base,
        &staged,
        &[],
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    write(
        &f.live,
        "var/lib/user-app/state",
        "late application write\n",
    );
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let candidate = update
        .refresh(&latest, &f.work.join("candidate"), |_| {
            panic!("known update-marker hook does not read this application state")
        })
        .unwrap();
    let oracle = f.work.join("oracle");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &oracle,
        ],
    );
    pacman(&oracle, &f.archive, &f.conf);
    assert_tree_matches_pacman(candidate.path(), &oracle);
    assert!(!candidate.stats.native_restage);
}

#[test]
fn ordinary_package_symlinks_do_not_disable_reuse() {
    if isolated("ordinary_package_symlinks_do_not_disable_reuse") {
        return;
    }
    let f = Fixture::new();
    let archive_path = f.work.join("linked.pkg.tar");
    let mut archive = tar::Builder::new(fs::File::create(&archive_path).unwrap());
    for (name, text) in [(".PKGINFO", "pkgname = pond-update-test\npkgver = 2-1\narch = any\nsize = 8\nbackup = etc/example.conf\n"), ("usr/share/example/version", "2"), ("etc/example.conf", "new\n")] {
        let mut header = tar::Header::new_gnu();
        header.set_size(text.len() as u64); header.set_uid(0); header.set_gid(0); header.set_mode(0o644); header.set_mtime(1_700_000_000); header.set_cksum();
        archive.append_data(&mut header, name, text.as_bytes()).unwrap();
    }
    let mut link = tar::Header::new_gnu();
    link.set_entry_type(tar::EntryType::Symlink);
    link.set_size(0);
    link.set_uid(0);
    link.set_gid(0);
    link.set_mode(0o777);
    link.set_mtime(1_700_000_000);
    archive
        .append_link(&mut link, "usr/share/example/current", "version")
        .unwrap();
    archive.finish().unwrap();
    drop(archive);
    let staging = f.work.join("link-staging");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &f.base,
            &staging,
        ],
    );
    pacman(&staging, &archive_path, &f.conf);
    let staged = f.work.join("link-staged");
    snapshot(&staging, &staged);
    let store = f.work.join("prepared");
    PreparedUpdate::prepare(
        &f.base,
        &staged,
        std::slice::from_ref(&archive_path),
        Path::new("var/lib/pacman"),
        &store,
    )
    .unwrap();
    let update = PreparedUpdate::open(&store).unwrap();
    write(&f.live, "var/lib/user-app/state", "late edit\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("symlink must remain reusable")
    };
    let oracle = f.work.join("oracle");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &oracle,
        ],
    );
    pacman(&oracle, &archive_path, &f.conf);
    assert_tree_matches_pacman(candidate.path(), &oracle);
}

#[test]
fn nondefault_file_handling_matches_native_pacman() {
    if isolated("nondefault_file_handling_matches_native_pacman") {
        return;
    }
    let f = Fixture::new();
    let mut config = fs::read_to_string(&f.conf).unwrap();
    config.push_str("NoExtract = usr/share/example/version\n");
    fs::write(&f.conf, &config).unwrap();
    write(&f.live, "etc/pacman.conf", &config);
    let base = f.work.join("config-base");
    snapshot(&f.live, &base);
    let staging = f.work.join("config-staging");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &base,
            &staging,
        ],
    );
    pacman(&staging, &f.archive, &f.conf);
    let staged = f.work.join("config-staged");
    snapshot(&staging, &staged);
    let update = PreparedUpdate::prepare(
        &base,
        &staged,
        &[],
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    write(
        &f.live,
        "usr/share/example/version",
        "preserve this local edit\n",
    );
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let candidate = update
        .refresh(&latest, &f.work.join("candidate"), |root| {
            pacman(root, &f.archive, &f.conf);
            Ok(())
        })
        .unwrap();
    assert!(candidate.stats.native_restage);
    let oracle = f.work.join("oracle");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &oracle,
        ],
    );
    pacman(&oracle, &f.archive, &f.conf);
    assert_tree_matches_pacman(candidate.path(), &oracle);
}

#[test]
fn replacing_an_unrelated_symlink_with_a_directory_never_writes_outside_candidate() {
    if isolated("replacing_an_unrelated_symlink_with_a_directory_never_writes_outside_candidate") {
        return;
    }
    use std::os::unix::fs::{symlink, PermissionsExt};
    let f = Fixture::new();
    let outside = f.work.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
    symlink(&outside, f.live.join("var/lib/user-app/replace")).unwrap();
    let base = f.work.join("link-base");
    snapshot(&f.live, &base);
    let staging = f.work.join("link-staging");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &base,
            &staging,
        ],
    );
    pacman(&staging, &f.archive, &f.conf);
    let staged = f.work.join("link-staged");
    snapshot(&staging, &staged);
    let update = PreparedUpdate::prepare(
        &base,
        &staged,
        &[],
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    fs::remove_file(f.live.join("var/lib/user-app/replace")).unwrap();
    fs::create_dir(f.live.join("var/lib/user-app/replace")).unwrap();
    fs::set_permissions(
        f.live.join("var/lib/user-app/replace"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("candidate")
    };
    assert_eq!(
        fs::metadata(&outside).unwrap().permissions().mode() & 0o777,
        0o755,
        "escaped into the symlink target"
    );
    assert!(!candidate
        .path()
        .join("var/lib/user-app/replace")
        .is_symlink());
    let oracle = f.work.join("oracle");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &oracle,
        ],
    );
    pacman(&oracle, &f.archive, &f.conf);
    assert_tree_matches_pacman(candidate.path(), &oracle);
}

#[test]
fn empty_nested_subvolume_snapshot_stubs_are_not_live_mounts() {
    if isolated("empty_nested_subvolume_snapshot_stubs_are_not_live_mounts") {
        return;
    }
    let f = Fixture::new();
    let nested = f.live.join("var/lib/portables");
    run(
        "btrfs",
        &[Path::new("subvolume"), Path::new("create"), &nested],
    );
    write(
        &nested,
        "independent-data",
        "keep this outside root snapshots\n",
    );
    let base = f.work.join("stub-base");
    snapshot(&f.live, &base);
    let staging = f.work.join("stub-staging");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &base,
            &staging,
        ],
    );
    pacman(&staging, &f.archive, &f.conf);
    let staged = f.work.join("stub-staged");
    snapshot(&staging, &staged);
    let update = PreparedUpdate::prepare(
        &base,
        &staged,
        &[],
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    write(&f.live, "var/lib/user-app/state", "late edit\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("empty snapshot stubs must be accepted")
    };
    assert!(fs::read_dir(candidate.path().join("var/lib/portables"))
        .unwrap()
        .next()
        .is_none());
    assert_eq!(
        fs::read_to_string(nested.join("independent-data")).unwrap(),
        "keep this outside root snapshots\n"
    );
}

#[test]
fn hooks_are_detected_when_pacmans_log_is_outside_the_snapshot() {
    if isolated("hooks_are_detected_when_pacmans_log_is_outside_the_snapshot") {
        return;
    }
    let f = Fixture::new();
    fixture_shell(&f.live);
    write(&f.live, "usr/share/libalpm/hooks/stateful.hook", "[Trigger]\nOperation = Upgrade\nType = Package\nTarget = pond-update-test\n[Action]\nWhen = PostTransaction\nExec = /bin/sh /usr/share/libalpm/scripts/stateful\n");
    write(&f.live, "usr/share/libalpm/scripts/stateful", "IFS= read -r value < /etc/hook-input\nprintf '%s\\n' \"$value\" > /usr/share/example/version\n");
    write(&f.live, "etc/hook-input", "before\n");
    let native = |root: &Path| {
        let output = Command::new("pacman")
            .arg("--root")
            .arg(root)
            .arg("--dbpath")
            .arg(root.join("var/lib/pacman"))
            .arg("--logfile")
            .arg(f.work.join("shared-pacman.log"))
            .arg("--config")
            .arg(&f.conf)
            .args(["--noconfirm", "-U"])
            .arg(&f.archive)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let base = f.work.join("hook-base");
    snapshot(&f.live, &base);
    let staging = f.work.join("hook-staging");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &base,
            &staging,
        ],
    );
    native(&staging);
    let staged = f.work.join("hook-staged");
    snapshot(&staging, &staged);
    assert_eq!(
        fs::read(base.join("var/log/pacman.log")).unwrap(),
        fs::read(staged.join("var/log/pacman.log")).unwrap()
    );
    let update = PreparedUpdate::prepare(
        &base,
        &staged,
        &[],
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    write(&f.live, "etc/hook-input", "after\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let candidate = update
        .refresh(&latest, &f.work.join("candidate"), |root| {
            native(root);
            Ok(())
        })
        .unwrap();
    assert!(candidate.stats.native_restage);
    assert_eq!(
        fs::read_to_string(candidate.path().join("usr/share/example/version")).unwrap(),
        "after\n"
    );
}

#[test]
#[ignore = "requires real CAP_SYS_ADMIN for bounded Btrfs delta discovery"]
fn finalization_carries_shutdown_writes_without_scanning_the_root() {
    if isolated("finalization_carries_shutdown_writes_without_scanning_the_root") {
        return;
    }
    use std::time::{Duration, Instant};
    let f = Fixture::with_background_files(Some("new\n"), None, 10_000);
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        &[],
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    let RebaseOutcome::Candidate(background) =
        update.rebase(&f.base, &f.work.join("background")).unwrap()
    else {
        panic!("background candidate")
    };
    // A shutdown writer atomically replaces a state file after preparation.
    write(&f.live, "var/lib/user-app/state.next", "shutdown state\n");
    fs::rename(
        f.live.join("var/lib/user-app/state.next"),
        f.live.join("var/lib/user-app/state"),
    )
    .unwrap();
    // New-file renames in a send stream must not gain a trailing slash and
    // accidentally force the sparse path to fall back to a scan.
    write(&f.live, "var/lib/user-app/new-at-shutdown", "new state\n");
    let final_root = f.work.join("final-root");
    snapshot(&f.live, &final_root);
    let start = Instant::now();
    let RebaseOutcome::Candidate(finalized) = update
        .finish(
            &background,
            &final_root,
            &f.work.join("finalized"),
            Duration::from_secs(1),
        )
        .unwrap()
    else {
        panic!("final candidate")
    };
    println!(
        "finalization with atomic shutdown write, 10k files: {:.3} ms",
        start.elapsed().as_secs_f64() * 1000.
    );
    assert_eq!(finalized.stats.scanned_paths, 0);
    let oracle = f.work.join("oracle");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &final_root,
            &oracle,
        ],
    );
    pacman(&oracle, &f.archive, &f.conf);
    assert_tree_matches_pacman(finalized.path(), &oracle);
    assert_eq!(
        finalized
            .validate(&final_root, Duration::from_secs(1))
            .unwrap(),
        libppm::update::Validation::Ready
    );
    assert_eq!(
        fs::read(f.live.join("usr/share/example/version")).unwrap(),
        b"1"
    );
    assert!(matches!(
        update
            .finish(
                &background,
                &final_root,
                &f.work.join("expired"),
                Duration::ZERO
            )
            .unwrap(),
        RebaseOutcome::NotReady(_)
    ));
    assert!(!f.work.join("expired").exists());
    let foreign = libppm::update::Candidate::from_completed(&f.base, &f.staged).unwrap();
    assert!(update
        .finish(
            &foreign,
            &final_root,
            &f.work.join("foreign"),
            Duration::from_secs(1)
        )
        .is_err());
    assert!(!f.work.join("foreign").exists());
    // A late live package transaction must not be overwritten by the staged DB.
    write(
        &f.live,
        "var/lib/pacman/local/ALPM_DB_VERSION",
        "late DB edit\n",
    );
    let changed_db = f.work.join("changed-db");
    snapshot(&f.live, &changed_db);
    assert!(matches!(
        update
            .finish(
                &background,
                &changed_db,
                &f.work.join("db-candidate"),
                Duration::from_secs(1)
            )
            .unwrap(),
        RebaseOutcome::NotReady(_)
    ));
    assert!(!f.work.join("db-candidate").exists());
}

#[test]
fn md5_collision_with_restored_mtime_preserves_live_state() {
    if isolated("md5_collision_with_restored_mtime_preserves_live_state") {
        return;
    }
    let decode = |hex: &str| -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    };
    // Published same-length collision: https://www.mscs.dal.ca/~selinger/md5collision/
    let a = decode(concat!(
        "d131dd02c5e6eec4693d9a0698aff95c2fcab58712467eab4004583eb8fb7f89",
        "55ad340609f4b30283e488832571415a085125e8f7cdc99fd91dbdf280373c5b",
        "d8823e3156348f5bae6dacd436c919c6dd53e2b487da03fd02396306d248cda0",
        "e99f33420f577ee8ce54b67080a80d1ec69821bcb6a8839396f9652b6ff72a70"
    ));
    let b = decode(concat!(
        "d131dd02c5e6eec4693d9a0698aff95c2fcab50712467eab4004583eb8fb7f89",
        "55ad340609f4b30283e4888325f1415a085125e8f7cdc99fd91dbd7280373c5b",
        "d8823e3156348f5bae6dacd436c919c6dd53e23487da03fd02396306d248cda0",
        "e99f33420f577ee8ce54b67080280d1ec69821bcb6a8839396f965ab6ff72a70"
    ));
    assert_ne!(a, b);
    assert_eq!(md5::compute(&a), md5::compute(&b));
    let f = Fixture::new();
    let path = f.live.join("var/lib/user-app/state");
    fs::write(&path, &a).unwrap();
    let mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let base = f.work.join("collision-base");
    snapshot(&f.live, &base);
    let staging = f.work.join("collision-staging");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &base,
            &staging,
        ],
    );
    pacman(&staging, &f.archive, &f.conf);
    let staged = f.work.join("collision-staged");
    snapshot(&staging, &staged);
    let update = PreparedUpdate::prepare(
        &base,
        &staged,
        &[],
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    fs::write(&path, &b).unwrap();
    fs::File::open(&path).unwrap().set_modified(mtime).unwrap();
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("candidate")
    };
    let oracle = f.work.join("oracle");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &oracle,
        ],
    );
    pacman(&oracle, &f.archive, &f.conf);
    assert_eq!(
        fs::read(candidate.path().join("var/lib/user-app/state")).unwrap(),
        b
    );
    assert_tree_matches_pacman(candidate.path(), &oracle);
}

#[test]
fn unmodeled_staging_effects_refuse_before_creating_a_candidate() {
    if isolated("unmodeled_staging_effects_refuse_before_creating_a_candidate") {
        return;
    }
    let f = Fixture::new();
    let modified = f.work.join("modified-stage");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &f.staged,
            &modified,
        ],
    );
    write(
        &modified,
        "var/lib/unmodeled/output",
        "derived from an unknown input\n",
    );
    let staged = f.work.join("modified-stage-frozen");
    snapshot(&modified, &staged);
    let update = PreparedUpdate::prepare(
        &f.base,
        &staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    let destination = f.work.join("candidate");
    assert!(matches!(
        update.rebase(&f.base, &destination).unwrap(),
        RebaseOutcome::NotReady(_)
    ));
    assert!(!destination.exists());
}

#[test]
fn a_prepared_update_survives_restarting_the_updater() {
    if isolated("a_prepared_update_survives_restarting_the_updater") {
        return;
    }
    let f = Fixture::new();
    let store = f.work.join("prepared");
    drop(
        PreparedUpdate::prepare(
            &f.base,
            &f.staged,
            std::slice::from_ref(&f.archive),
            Path::new("var/lib/pacman"),
            &store,
        )
        .unwrap(),
    );
    write(&f.live, "etc/example.conf", "after restart\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let update = PreparedUpdate::open(&store).unwrap();
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("expected candidate")
    };
    assert_eq!(
        fs::read_to_string(candidate.path().join("etc/example.conf")).unwrap(),
        "after restart\n"
    );
    assert_eq!(
        fs::read_to_string(candidate.path().join("etc/example.conf.pacnew")).unwrap(),
        "new\n"
    );
}

#[test]
fn unrelated_extended_attribute_changes_are_preserved() {
    if isolated("unrelated_extended_attribute_changes_are_preserved") {
        return;
    }
    use std::{ffi::CString, os::unix::ffi::OsStrExt};
    let f = Fixture::new();
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    let path = CString::new(f.live.join("var/lib/user-app/state").as_os_str().as_bytes()).unwrap();
    let key = c"user.pond-test";
    assert_eq!(
        unsafe { libc::setxattr(path.as_ptr(), key.as_ptr(), b"latest".as_ptr().cast(), 6, 0) },
        0
    );
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("expected candidate")
    };
    let path = CString::new(
        candidate
            .path()
            .join("var/lib/user-app/state")
            .as_os_str()
            .as_bytes(),
    )
    .unwrap();
    let mut value = [0u8; 6];
    assert_eq!(
        unsafe {
            libc::getxattr(
                path.as_ptr(),
                key.as_ptr(),
                value.as_mut_ptr().cast(),
                value.len(),
            )
        },
        6
    );
    assert_eq!(&value, b"latest");
}

#[test]
fn concurrent_log_appends_are_kept_and_transaction_is_appended_once() {
    if isolated("concurrent_log_appends_are_kept_and_transaction_is_appended_once") {
        return;
    }
    use std::io::Write;
    let f = Fixture::new();
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    writeln!(
        fs::OpenOptions::new()
            .append(true)
            .open(f.live.join("var/log/pacman.log"))
            .unwrap(),
        "user's latest log entry"
    )
    .unwrap();
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("expected candidate")
    };
    let log = fs::read_to_string(candidate.path().join("var/log/pacman.log")).unwrap();
    assert!(log.contains("user's latest log entry"));
    assert_eq!(
        log.matches("upgraded pond-update-test (1-1 -> 2-1)")
            .count(),
        1
    );
}

#[test]
fn deleting_unrelated_state_directories_is_preserved() {
    if isolated("deleting_unrelated_state_directories_is_preserved") {
        return;
    }
    let f = Fixture::new();
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    fs::remove_file(f.live.join("var/lib/user-app/state")).unwrap();
    fs::remove_dir(f.live.join("var/lib/user-app")).unwrap();
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("expected candidate")
    };
    assert!(!candidate.path().join("var/lib/user-app").exists());
}

#[test]
fn changing_a_frozen_staging_input_invalidates_the_preparation() {
    if isolated("changing_a_frozen_staging_input_invalidates_the_preparation") {
        return;
    }
    let f = Fixture::new();
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    run(
        "btrfs",
        &[
            Path::new("property"),
            Path::new("set"),
            &f.staged,
            Path::new("ro"),
            Path::new("false"),
        ],
    );
    write(&f.staged, "usr/share/example/version", "tampered");
    run(
        "btrfs",
        &[
            Path::new("property"),
            Path::new("set"),
            &f.staged,
            Path::new("ro"),
            Path::new("true"),
        ],
    );
    let destination = f.work.join("candidate");
    assert!(matches!(
        update.rebase(&f.base, &destination).unwrap(),
        RebaseOutcome::NotReady(_)
    ));
    assert!(!destination.exists());
}

#[test]
fn completed_candidates_are_sealed_read_only() {
    if isolated("completed_candidates_are_sealed_read_only") {
        return;
    }
    let f = Fixture::new();
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&f.base, &f.work.join("candidate")).unwrap()
    else {
        panic!("expected candidate")
    };
    assert!(fs::write(candidate.path().join("usr/share/example/version"), "oops").is_err());
    assert_eq!(
        fs::read_to_string(candidate.path().join("usr/share/example/version")).unwrap(),
        "2"
    );
}

#[test]
fn damaged_cached_payload_is_not_published() {
    if isolated("damaged_cached_payload_is_not_published") {
        return;
    }
    let f = Fixture::with_initial_edit(Some("old\n"), Some("custom\n"));
    let store = f.work.join("prepared");
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &store,
    )
    .unwrap();
    for file in fs::read_dir(store.join("package-0")).unwrap() {
        fs::write(file.unwrap().path(), "damaged cache").unwrap();
    }
    fs::remove_file(f.live.join("etc/example.conf")).unwrap();
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let destination = f.work.join("candidate");
    assert!(update.rebase(&latest, &destination).is_err());
    assert!(!destination.exists());
}

#[test]
fn backup_already_equal_to_new_preserves_its_metadata_like_pacman() {
    if isolated("backup_already_equal_to_new_preserves_its_metadata_like_pacman") {
        return;
    }
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let f = Fixture::new();
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    write(&f.live, "etc/example.conf", "new\n");
    fs::set_permissions(
        f.live.join("etc/example.conf"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let oracle = f.work.join("oracle");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &oracle,
        ],
    );
    pacman(&oracle, &f.archive, &f.conf);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("expected candidate")
    };
    let actual = fs::metadata(candidate.path().join("etc/example.conf")).unwrap();
    let expected = fs::metadata(oracle.join("etc/example.conf")).unwrap();
    assert_eq!(actual.mode(), expected.mode());
    assert_eq!(
        (actual.mtime(), actual.mtime_nsec()),
        (expected.mtime(), expected.mtime_nsec())
    );
    assert_tree_matches_pacman(candidate.path(), &oracle);
}

#[test]
fn timestamp_only_changes_to_unrelated_files_are_preserved() {
    if isolated("timestamp_only_changes_to_unrelated_files_are_preserved") {
        return;
    }
    let f = Fixture::new();
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    fs::File::open(f.live.join("var/lib/user-app/state"))
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(123))
        .unwrap();
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let oracle = f.work.join("oracle");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &oracle,
        ],
    );
    pacman(&oracle, &f.archive, &f.conf);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("expected candidate")
    };
    assert_tree_matches_pacman(candidate.path(), &oracle);
}

#[test]
fn existing_pacsave_is_deferred_instead_of_overwritten() {
    if isolated("existing_pacsave_is_deferred_instead_of_overwritten") {
        return;
    }
    let f = Fixture::with_config(None);
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    write(&f.live, "etc/example.conf", "latest user edit\n");
    write(
        &f.live,
        "etc/example.conf.pacsave",
        "previously saved edit\n",
    );
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let destination = f.work.join("candidate");
    assert!(matches!(
        update.rebase(&latest, &destination).unwrap(),
        RebaseOutcome::NotReady(_)
    ));
    assert!(!destination.exists());
    assert_eq!(
        fs::read_to_string(f.live.join("etc/example.conf.pacsave")).unwrap(),
        "previously saved edit\n"
    );
}

#[test]
fn damaged_cached_permissions_are_not_published() {
    if isolated("damaged_cached_permissions_are_not_published") {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let f = Fixture::with_initial_edit(Some("old\n"), Some("custom\n"));
    let store = f.work.join("prepared");
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &store,
    )
    .unwrap();
    for file in fs::read_dir(store.join("package-0")).unwrap() {
        fs::set_permissions(file.unwrap().path(), fs::Permissions::from_mode(0o777)).unwrap();
    }
    fs::remove_file(f.live.join("etc/example.conf")).unwrap();
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let destination = f.work.join("candidate");
    assert!(update.rebase(&latest, &destination).is_err());
    assert!(!destination.exists());
}

#[test]
fn cli_prepares_and_rebases_a_private_candidate() {
    if isolated("cli_prepares_and_rebases_a_private_candidate") {
        return;
    }
    let f = Fixture::new();
    let store = f.work.join("prepared");
    let destination = f.work.join("candidate");
    let program = Path::new(env!("CARGO_BIN_EXE_pond-update"));
    let prepared = Command::new(program)
        .arg("prepare")
        .arg(&f.base)
        .arg(&f.staged)
        .arg("var/lib/pacman")
        .arg(&store)
        .arg(&f.archive)
        .output()
        .unwrap();
    assert!(
        prepared.status.success(),
        "{}",
        String::from_utf8_lossy(&prepared.stderr)
    );
    let result = Command::new(program)
        .arg("rebase")
        .arg(&store)
        .arg(&f.base)
        .arg(&destination)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json["status"], "candidate");
    assert_eq!(
        fs::read_to_string(destination.join("usr/share/example/version")).unwrap(),
        "2"
    );
}

#[test]
fn unsupported_divergence_never_creates_a_candidate() {
    if isolated("unsupported_divergence_never_creates_a_candidate") {
        return;
    }
    use std::os::unix::fs::symlink;
    for case in [
        "database",
        "configuration",
        "hardlink",
        "target-type",
        "symlink-ancestor",
        "special-file",
    ] {
        let f = Fixture::new();
        let update = PreparedUpdate::prepare(
            &f.base,
            &f.staged,
            std::slice::from_ref(&f.archive),
            Path::new("var/lib/pacman"),
            &f.work.join("prepared"),
        )
        .unwrap();
        match case {
            "special-file" => run("mkfifo", &[&f.live.join("var/lib/user-app/pipe")]),
            "database" => write(
                &f.live,
                "var/lib/pacman/local/pond-update-test-1-1/desc",
                "changed package state\n",
            ),
            "configuration" => write(&f.live, "etc/pacman.conf", "[options]\nNoUpgrade = *\n"),
            "hardlink" => fs::hard_link(
                f.live.join("usr/share/example/version"),
                f.live.join("alias"),
            )
            .unwrap(),
            "target-type" => {
                fs::remove_file(f.live.join("usr/share/example/version")).unwrap();
                fs::create_dir(f.live.join("usr/share/example/version")).unwrap();
            }
            "symlink-ancestor" => {
                fs::rename(f.live.join("etc"), f.live.join("old-etc")).unwrap();
                symlink("old-etc", f.live.join("etc")).unwrap();
            }
            _ => unreachable!(),
        }
        let latest = f.work.join("latest");
        snapshot(&f.live, &latest);
        let destination = f.work.join("candidate");
        assert!(
            !matches!(
                update.rebase(&latest, &destination),
                Ok(RebaseOutcome::Candidate(_))
            ),
            "{case}"
        );
        assert!(!destination.exists(), "{case}");
    }
}

#[test]
fn existing_output_and_live_root_inputs_are_never_modified() {
    if isolated("existing_output_and_live_root_inputs_are_never_modified") {
        return;
    }
    let f = Fixture::new();
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    let destination = f.work.join("candidate");
    assert!(update.rebase(&f.live, &destination).is_err());
    assert!(!destination.exists());
    fs::create_dir(&destination).unwrap();
    fs::write(destination.join("marker"), "keep").unwrap();
    assert!(update.rebase(&f.base, &destination).is_err());
    assert_eq!(
        fs::read_to_string(destination.join("marker")).unwrap(),
        "keep"
    );
}

#[test]
fn mismatched_archive_backup_does_not_get_certified() {
    if isolated("mismatched_archive_backup_does_not_get_certified") {
        return;
    }
    let f = Fixture::with_initial_edit(Some("old\n"), Some("custom\n"));
    // Staging is now the caller-attested native output, not something to
    // certify by re-extracting all payload. Optional backup archives must still
    // match the staged database before their bytes can repair a candidate.
    package(&f.archive, "2", Some("not the staged package default\n"));
    let destination = f.work.join("candidate");
    let result = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    );
    if let Ok(update) = result {
        assert!(matches!(
            update.rebase(&f.base, &destination).unwrap(),
            RebaseOutcome::NotReady(_)
        ));
    }
    assert!(!destination.exists());
}

#[test]
fn backup_decision_matrix_matches_native_pacman() {
    if isolated("backup_decision_matrix_matches_native_pacman") {
        return;
    }
    for incoming in [Some("old\n"), Some("new\n"), None] {
        for current in [Some("old\n"), Some("new\n"), Some("user edit\n"), None] {
            let f = Fixture::with_config(incoming);
            let update = PreparedUpdate::prepare(
                &f.base,
                &f.staged,
                std::slice::from_ref(&f.archive),
                Path::new("var/lib/pacman"),
                &f.work.join("prepared"),
            )
            .unwrap();
            match current {
                Some(text) => write(&f.live, "etc/example.conf", text),
                None => fs::remove_file(f.live.join("etc/example.conf")).unwrap(),
            }
            let latest = f.work.join("latest");
            snapshot(&f.live, &latest);
            let oracle = f.work.join("oracle");
            run(
                "btrfs",
                &[
                    Path::new("subvolume"),
                    Path::new("snapshot"),
                    &latest,
                    &oracle,
                ],
            );
            pacman(&oracle, &f.archive, &f.conf);
            let RebaseOutcome::Candidate(candidate) =
                update.rebase(&latest, &f.work.join("candidate")).unwrap()
            else {
                panic!("expected candidate for {incoming:?}/{current:?}")
            };
            assert_tree_matches_pacman(candidate.path(), &oracle);
        }
    }
}

#[test]
fn root_directory_metadata_is_preserved() {
    if isolated("root_directory_metadata_is_preserved") {
        return;
    }
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let f = Fixture::new();
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    fs::set_permissions(&f.live, fs::Permissions::from_mode(0o750)).unwrap();
    xattr::set(&f.live, "user.pond-test", b"root metadata").unwrap();
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("expected candidate")
    };
    assert_eq!(
        fs::metadata(candidate.path()).unwrap().mode(),
        fs::metadata(&latest).unwrap().mode()
    );
    assert_eq!(
        xattr::get(candidate.path(), "user.pond-test").unwrap(),
        Some(b"root metadata".to_vec())
    );
}

#[test]
#[ignore = "explicit larger-tree timing run; requires Btrfs and native pacman"]
fn larger_tree_baseline() {
    if isolated("larger_tree_baseline") {
        return;
    }
    let f = Fixture::with_background_files(Some("new\n"), None, 10_000);
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    write(&f.live, "etc/example.conf", "late user edit\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    for iteration in 0..5 {
        let RebaseOutcome::Candidate(candidate) = update
            .rebase(&latest, &f.work.join(format!("candidate-{iteration}")))
            .unwrap()
        else {
            panic!("expected candidate")
        };
        assert_eq!(candidate.stats.repaired_files, 1);
        println!("{}", serde_json::to_string(&candidate.stats).unwrap());
    }
}

#[test]
fn late_default_acl_changes_require_native_restaging() {
    if isolated("late_default_acl_changes_require_native_restaging") {
        return;
    }
    let f = Fixture::new();
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    run(
        "setfacl",
        &[
            Path::new("-m"),
            Path::new("d:u::rwx,d:g::r-x,d:o::---"),
            &f.live.join("etc"),
        ],
    );
    write(&f.live, "etc/example.conf", "late edit\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let destination = f.work.join("candidate");
    assert!(matches!(
        update.rebase(&latest, &destination).unwrap(),
        RebaseOutcome::NotReady(_)
    ));
    assert!(!destination.exists());
}

#[test]
fn concurrent_publishers_cannot_replace_each_others_candidate() {
    if isolated("concurrent_publishers_cannot_replace_each_others_candidate") {
        return;
    }
    let f = Fixture::new();
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    let first = f.work.join("latest-first");
    write(&f.live, "etc/example.conf", "first edit\n");
    snapshot(&f.live, &first);
    let second = f.work.join("latest-second");
    write(&f.live, "etc/example.conf", "second edit\n");
    snapshot(&f.live, &second);
    let destination = f.work.join("candidate");
    let barrier = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            barrier.wait();
            update.rebase(&first, &destination)
        });
        let b = scope.spawn(|| {
            barrier.wait();
            update.rebase(&second, &destination)
        });
        let results = [a.join().unwrap(), b.join().unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, Ok(RebaseOutcome::Candidate(_))))
                .count(),
            1
        );
        for (result, content) in results.iter().zip(["first edit\n", "second edit\n"]) {
            if let Ok(RebaseOutcome::Candidate(candidate)) = result {
                assert_eq!(
                    fs::read_to_string(candidate.path().join("etc/example.conf")).unwrap(),
                    content
                );
                assert_eq!(
                    fs::read_to_string(candidate.path().join("etc/example.conf.pacnew")).unwrap(),
                    "new\n"
                );
            }
        }
    });
}

#[test]
fn cli_returns_a_distinct_not_ready_status() {
    if isolated("cli_returns_a_distinct_not_ready_status") {
        return;
    }
    let f = Fixture::new();
    let store = f.work.join("prepared");
    PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &store,
    )
    .unwrap();
    write(&f.live, "etc/pacman.conf", "[options]\nNoUpgrade = *\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let destination = f.work.join("candidate");
    let output = Command::new(env!("CARGO_BIN_EXE_pond-update"))
        .arg("rebase")
        .arg(&store)
        .arg(&latest)
        .arg(&destination)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["status"], "not-ready");
    assert!(!destination.exists());
}

#[test]
fn removed_backup_saves_the_latest_user_edit() {
    if isolated("removed_backup_saves_the_latest_user_edit") {
        return;
    }
    let f = Fixture::with_config(None);
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    write(&f.live, "etc/example.conf", "late edit\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let oracle = f.work.join("oracle");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &oracle,
        ],
    );
    pacman(&oracle, &f.archive, &f.conf);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("expected candidate")
    };
    assert!(!candidate.path().join("etc/example.conf").exists());
    assert_eq!(
        fs::read(candidate.path().join("etc/example.conf.pacsave")).unwrap(),
        fs::read(oracle.join("etc/example.conf.pacsave")).unwrap()
    );
    assert_eq!(
        fs::read_to_string(f.live.join("etc/example.conf")).unwrap(),
        "late edit\n"
    );
    assert_tree_matches_pacman(candidate.path(), &oracle);
}

#[test]
fn late_config_edit_matches_pacman_without_mutating_live_root() {
    if isolated("late_config_edit_matches_pacman_without_mutating_live_root") {
        return;
    }
    let f = Fixture::new();
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    write(&f.live, "etc/example.conf", "user edit\n");
    write(&f.live, "var/lib/user-app/state", "latest\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let oracle = f.work.join("oracle");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &oracle,
        ],
    );
    pacman(&oracle, &f.archive, &f.conf);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("expected fast candidate")
    };
    for path in [
        "etc/example.conf",
        "etc/example.conf.pacnew",
        "usr/share/example/version",
        "var/lib/user-app/state",
    ] {
        assert_eq!(
            fs::read(candidate.path().join(path)).unwrap(),
            fs::read(oracle.join(path)).unwrap(),
            "{path}"
        );
    }
    assert_eq!(
        fs::read_to_string(f.live.join("etc/example.conf")).unwrap(),
        "user edit\n"
    );
    assert_eq!(
        fs::read_to_string(f.live.join("usr/share/example/version")).unwrap(),
        "1"
    );
    assert!(!f.live.join("etc/example.conf.pacnew").exists());
    assert_tree_matches_pacman(candidate.path(), &oracle);
}

#[test]
fn background_fallback_runs_native_pacman_on_the_latest_root() {
    if isolated("background_fallback_runs_native_pacman_on_the_latest_root") {
        return;
    }
    let f = Fixture::new();
    let update = PreparedUpdate::prepare(
        &f.base,
        &f.staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    // An ordinary, successful live pacman transaction happened after staging.
    let third = f.work.join("third.pkg.tar");
    package(&third, "3", Some("third\n"));
    pacman(&f.live, &third, &f.conf);
    write(
        &f.live,
        "var/lib/user-app/state",
        "latest application state\n",
    );
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let mut called = false;
    let candidate = update
        .refresh(&latest, &f.work.join("candidate"), |private| {
            called = true;
            assert_ne!(private, f.live);
            pacman(private, &f.archive, &f.conf);
            Ok(())
        })
        .unwrap();
    assert!(called);
    assert!(candidate.stats.native_restage);
    let expected = f.work.join("expected");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &expected,
        ],
    );
    pacman(&expected, &f.archive, &f.conf);
    assert_tree_matches_pacman(candidate.path(), &expected);
    assert_tree_matches_pacman(&f.live, &latest);
}

#[test]
fn failed_native_fallback_never_publishes_partial_changes() {
    if isolated("failed_native_fallback_never_publishes_partial_changes") {
        return;
    }
    let f = Fixture::new();
    let destination = f.work.join("candidate");
    let result = libppm::update::restage(&f.base, &destination, |private| {
        write(private, "etc/example.conf", "partially installed\n");
        anyhow::bail!("native transaction failed")
    });
    assert!(result.is_err());
    assert!(!destination.exists());
    assert_eq!(
        fs::read_to_string(f.base.join("etc/example.conf")).unwrap(),
        "old\n"
    );
    // Unprivileged Btrfs deletion may be disallowed by the host mount. Such a
    // private working tree can remain for GC; it must never have a receipt.
    assert!(!f.work.join("candidate.json").exists());
}

#[test]
fn candidate_receipt_pins_both_output_and_reference() {
    if isolated("candidate_receipt_pins_both_output_and_reference") {
        return;
    }
    use libppm::update::{Candidate, Validation};
    use std::time::Duration;
    let f = Fixture::new();
    let candidate = Candidate::from_completed(&f.base, &f.staged).unwrap();
    let receipt = f.work.join("candidate.json");
    candidate.save(&receipt).unwrap();
    assert!(candidate.save(&receipt).is_err());
    let candidate = Candidate::open(&receipt).unwrap();
    assert_eq!(
        candidate.validate(&f.base, Duration::from_secs(1)).unwrap(),
        Validation::Ready
    );
    assert_eq!(
        candidate.validate(&f.base, Duration::ZERO).unwrap(),
        Validation::TimedOut
    );
    run(
        "btrfs",
        &[
            Path::new("property"),
            Path::new("set"),
            &f.staged,
            Path::new("ro"),
            Path::new("false"),
        ],
    );
    write(&f.staged, "usr/share/example/version", "damaged candidate");
    run(
        "btrfs",
        &[
            Path::new("property"),
            Path::new("set"),
            &f.staged,
            Path::new("ro"),
            Path::new("true"),
        ],
    );
    assert!(candidate.validate(&f.base, Duration::from_secs(1)).is_err());
    assert!(Candidate::open(&receipt).is_err());
}

#[test]
#[ignore = "requires CAP_SYS_ADMIN on the Btrfs filesystem for send; run the privileged fixture test"]
fn final_validation_detects_late_writes_without_hashing_payloads() {
    if isolated("final_validation_detects_late_writes_without_hashing_payloads") {
        return;
    }
    use libppm::update::{Candidate, Validation};
    use std::time::{Duration, Instant};
    let f = Fixture::with_background_files(Some("new\n"), None, 10_000);
    let candidate = Candidate::from_completed(&f.base, &f.staged).unwrap();
    let final_root = f.work.join("final");
    snapshot(&f.live, &final_root);
    let start = Instant::now();
    assert_eq!(
        candidate
            .validate(&final_root, Duration::from_secs(5))
            .unwrap(),
        Validation::Ready
    );
    eprintln!(
        "unchanged final validation, 10k files: {:.3} ms",
        start.elapsed().as_secs_f64() * 1000.
    );
    write(&f.live, "var/lib/user-app/state", "late write\n");
    let changed = f.work.join("changed");
    snapshot(&f.live, &changed);
    assert_eq!(
        candidate
            .validate(&changed, Duration::from_secs(5))
            .unwrap(),
        Validation::Changed
    );
    assert_eq!(
        candidate
            .validate(&changed, Duration::from_nanos(1))
            .unwrap(),
        Validation::TimedOut
    );
    // Even unchanged size+mtime must not hide content edits.
    let file = f.live.join("usr/share/example/version");
    let old_mtime = fs::metadata(&file).unwrap().modified().unwrap();
    fs::write(&file, "x").unwrap();
    fs::File::open(&file)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(old_mtime))
        .unwrap();
    let rewritten = f.work.join("rewritten");
    snapshot(&f.live, &rewritten);
    assert_eq!(
        candidate
            .validate(&rewritten, Duration::from_secs(5))
            .unwrap(),
        Validation::Changed
    );
    // Use a fresh baseline so the inode flag is the ONLY substantive difference.
    let flags_candidate = Candidate::from_completed(&rewritten, &f.staged).unwrap();
    run("chattr", &[Path::new("+i"), &file]);
    let flagged = f.work.join("flagged");
    snapshot(&f.live, &flagged);
    assert_eq!(
        flags_candidate
            .validate(&flagged, Duration::from_secs(5))
            .unwrap(),
        Validation::Changed
    );
    run("chattr", &[Path::new("-i"), &file]);
}

#[test]
fn scriptlet_restage_reads_latest_application_state() {
    if isolated("scriptlet_restage_reads_latest_application_state") {
        return;
    }
    let f = Fixture::new();
    fixture_shell(&f.live);
    let base = f.work.join("script-base");
    snapshot(&f.live, &base);
    let archive = f.work.join("script.pkg.tar");
    package_with_scriptlet(&archive, "2", Some("new\n"), Some(
        "post_upgrade() {\n IFS= read -r state < /var/lib/user-app/state\n printf '%s\\n' \"$state\" > /usr/share/example/generated\n /usr/bin/touch -d @1700000000 /usr/share/example/generated\n}\n"
    ));
    let staging = f.work.join("script-staging");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &base,
            &staging,
        ],
    );
    pacman(&staging, &archive, &f.conf);
    assert_eq!(
        fs::read_to_string(staging.join("usr/share/example/generated")).unwrap(),
        "original\n"
    );
    let staged = f.work.join("script-staged");
    snapshot(&staging, &staged);
    let update = PreparedUpdate::prepare(
        &base,
        &staged,
        std::slice::from_ref(&archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    let reused = update
        .refresh(&base, &f.work.join("unchanged-candidate"), |_| {
            panic!("an already completed transaction must not rerun on its exact original base")
        })
        .unwrap();
    assert!(!reused.stats.native_restage);
    assert_tree_matches_pacman(reused.path(), &staged);
    write(&f.live, "var/lib/user-app/state", "latest state\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let candidate = update
        .refresh(&latest, &f.work.join("candidate"), |private| {
            pacman(private, &archive, &f.conf);
            Ok(())
        })
        .unwrap();
    assert!(candidate.stats.native_restage);
    assert_eq!(
        fs::read_to_string(candidate.path().join("usr/share/example/generated")).unwrap(),
        "latest state\n"
    );
    let expected = f.work.join("expected");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &expected,
        ],
    );
    pacman(&expected, &archive, &f.conf);
    assert_tree_matches_pacman(candidate.path(), &expected);
    assert_tree_matches_pacman(&latest, &f.live);
}

#[test]
fn cli_captures_and_validates_a_completed_transaction() {
    if isolated("cli_captures_and_validates_a_completed_transaction") {
        return;
    }
    let f = Fixture::new();
    let receipt = f.work.join("candidate.json");
    let program = env!("CARGO_BIN_EXE_pond-update");
    let output = Command::new(program)
        .arg("capture")
        .arg(&f.base)
        .arg(&f.staged)
        .arg(&receipt)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for (budget, expected, status) in [("1000", "ready", 0), ("0", "timed-out", 3)] {
        let output = Command::new(program)
            .arg("validate")
            .arg(&receipt)
            .arg(&f.base)
            .arg(budget)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(status),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["status"], expected);
    }
}

#[test]
fn shared_subvolumes_stay_outside_whole_root_reconciliation() {
    if isolated("shared_subvolumes_stay_outside_whole_root_reconciliation") {
        return;
    }
    let f = Fixture::new();
    let shared = ["nix", "var/lib/flatpak", "usr/local"];
    for (index, path) in shared.iter().enumerate() {
        // Pond mounts sibling subvolumes over ordinary directories in @. A root
        // snapshot includes the underlying directory, not the mounted data.
        let source = f.work.join(format!("shared-{index}"));
        run(
            "btrfs",
            &[Path::new("subvolume"), Path::new("create"), &source],
        );
        fs::create_dir_all(f.live.join(path)).unwrap();
        run("mount", &[Path::new("--bind"), &source, &f.live.join(path)]);
        write(&f.live, &format!("{path}/independent-state"), "before\n");
    }
    let base = f.work.join("mounted-base");
    snapshot(&f.live, &base);
    let staging = f.work.join("mounted-staging");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &base,
            &staging,
        ],
    );
    pacman(&staging, &f.archive, &f.conf);
    let staged = f.work.join("mounted-staged");
    snapshot(&staging, &staged);
    let update = PreparedUpdate::prepare(
        &base,
        &staged,
        std::slice::from_ref(&f.archive),
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    for path in shared {
        write(
            &f.live,
            &format!("{path}/independent-state"),
            "independent update\n",
        );
    }
    write(&f.live, "etc/example.conf", "latest local config\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let RebaseOutcome::Candidate(candidate) =
        update.rebase(&latest, &f.work.join("candidate")).unwrap()
    else {
        panic!("ordinary root changes should be repairable");
    };
    for path in shared {
        assert!(!candidate
            .path()
            .join(path)
            .join("independent-state")
            .exists());
        assert_eq!(
            fs::read_to_string(f.live.join(path).join("independent-state")).unwrap(),
            "independent update\n"
        );
        run("umount", &[&f.live.join(path)]);
    }
    assert!(candidate
        .path()
        .join("var/lib/pacman/local/pond-update-test-2-1")
        .is_dir());
    assert!(latest
        .join("var/lib/pacman/local/pond-update-test-1-1")
        .is_dir());
    assert_eq!(
        fs::read_to_string(candidate.path().join("etc/example.conf")).unwrap(),
        "latest local config\n"
    );
}

#[test]
fn native_executor_cannot_publish_with_a_leftover_bind_mount() {
    if isolated("native_executor_cannot_publish_with_a_leftover_bind_mount") {
        return;
    }
    let f = Fixture::new();
    let destination = f.work.join("candidate");
    let result = libppm::update::restage(&f.base, &destination, |private| {
        let mount = private.join("etc/loop");
        fs::create_dir(&mount)?;
        // Same subvolume and st_dev, even an ancestor loop: mount IDs must catch it.
        run(
            "mount",
            &[Path::new("--bind"), &private.join("etc"), &mount],
        );
        Ok(())
    });
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("nested mount or subvolume"));
    assert!(!destination.exists());
    // The private namespace is destroyed when this isolated test process exits.
}

#[test]
fn removals_use_native_fallback_and_save_the_latest_backup() {
    if isolated("removals_use_native_fallback_and_save_the_latest_backup") {
        return;
    }
    let f = Fixture::new();
    let remove = |root: &Path| {
        let output = Command::new("pacman")
            .arg("--root")
            .arg(root)
            .arg("--dbpath")
            .arg(root.join("var/lib/pacman"))
            .arg("--logfile")
            .arg(root.join("var/log/pacman.log"))
            .arg("--config")
            .arg(&f.conf)
            .args(["--noconfirm", "-R", "pond-update-test"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let staging = f.work.join("remove-staging");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &f.base,
            &staging,
        ],
    );
    remove(&staging);
    let staged = f.work.join("remove-staged");
    snapshot(&staging, &staged);
    let update = PreparedUpdate::prepare(
        &f.base,
        &staged,
        &[],
        Path::new("var/lib/pacman"),
        &f.work.join("prepared"),
    )
    .unwrap();
    write(&f.live, "etc/example.conf", "late edit before removal\n");
    let latest = f.work.join("latest");
    snapshot(&f.live, &latest);
    let candidate = update
        .refresh(&latest, &f.work.join("candidate"), |private| {
            remove(private);
            Ok(())
        })
        .unwrap();
    assert!(candidate.stats.native_restage);
    assert_eq!(
        fs::read_to_string(candidate.path().join("etc/example.conf.pacsave")).unwrap(),
        "late edit before removal\n"
    );
    let expected = f.work.join("expected");
    run(
        "btrfs",
        &[
            Path::new("subvolume"),
            Path::new("snapshot"),
            &latest,
            &expected,
        ],
    );
    remove(&expected);
    assert_tree_matches_pacman(candidate.path(), &expected);
    assert_tree_matches_pacman(&latest, &f.live);
}
