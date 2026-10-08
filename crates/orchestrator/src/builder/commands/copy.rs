use tempfile::NamedTempFile;

use super::script::{COPY_SCRIPT, render};
use super::{CommandRun, as_root, user_of};
use crate::builder::index::{join_path, layer_files_path};
use crate::builder::sandbox_command::{ROOT, run_quiet};
use crate::envd::files::copy_file;
use crate::proto::{LogLevel, TemplateStep};
use crate::template::metadata::Context;

const UNPACK_LOG_ID: &str = "unpack";
const META: [char; 6] = ['*', '?', '[', ']', '{', '}'];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyArgs {
    pub source_path: String,
    pub target_path: String,
    pub owner: String,
    pub permissions: String,
}

pub async fn execute(run: &CommandRun<'_>, step: &TemplateStep, context: Context) -> Result<Context, String> {
    let args = CopyArgs::parse(&step.args, &user_of(&context))?;
    let hash = step
        .files_hash
        .as_deref()
        .filter(|hash| !hash.is_empty())
        .ok_or_else(|| format!("{} requires files hash to be set", step.r#type.to_uppercase()))?;
    let local = download(run, hash).await?;
    let tar = join_path(&["/tmp", &format!("{hash}.tar")]);
    copy_file(run.envd, ROOT, local.path(), &tar)
        .await
        .map_err(|e| format!("failed to copy layer tar data to sandbox: {e}"))?;
    let unpack = join_path(&["/tmp", hash, "unpack"]);
    let extract = format!("mkdir -p \"{unpack}\" && tar -xzvf \"{tar}\" -C \"{unpack}\"");
    run_quiet(run.envd, &extract, &as_root(&context))
        .await
        .map_err(|e| format!("failed to extract files: {e}"))?;
    move_files(run, &context, &args, &unpack).await?;
    Ok(context)
}

async fn download(run: &CommandRun<'_>, hash: &str) -> Result<NamedTempFile, String> {
    let path = layer_files_path(run.cache_scope, hash);
    run.files
        .open_blob(&path)
        .map_err(|e| format!("failed to open files object from storage: {e}"))?;
    let local = crate::go_os::create_temp("layer-file-", ".tar")
        .map_err(|e| format!("failed to create temporary file for layer tar: {e}"))?;
    run.files
        .write_to(&path, local.path())
        .await
        .map_err(|e| format!("failed to copy layer tar data to temporary file: {e}"))?;
    Ok(local)
}

async fn move_files(run: &CommandRun<'_>, context: &Context, args: &CopyArgs, unpack: &str) -> Result<(), String> {
    let workdir = context.workdir.clone().unwrap_or_default();
    let user = user_of(context);
    let source_path = join_path(&[unpack, &args.source_path]);
    let script = render(
        COPY_SCRIPT,
        &[
            ("TargetPath", &args.target_path),
            ("SourcePath", &source_path),
            ("Owner", &args.owner),
            ("Permissions", &args.permissions),
            ("Workdir", &workdir),
            ("User", &user),
        ],
    )
    .map_err(|e| format!("failed to execute copy script template: {e}"))?;
    let unpack_log = CommandRun {
        level: LogLevel::Debug,
        prefix: UNPACK_LOG_ID,
        ..*run
    };
    unpack_log
        .logged(&script, &as_root(context))
        .await
        .map_err(|e| format!("failed to move files in sandbox: {e}"))
}

impl CopyArgs {
    pub fn parse(args: &[String], default_user: &str) -> Result<Self, String> {
        let [source, target, rest @ ..] = args else {
            return Err("COPY requires a local path and a container path argument".to_owned());
        };
        let owner = rest
            .first()
            .filter(|owner| !owner.is_empty())
            .map_or_else(|| format!("{default_user}:{default_user}"), |owner| with_group(owner));
        Ok(Self {
            source_path: split_pattern(&ensure_trailing_slash(source)),
            target_path: target.clone(),
            owner,
            permissions: rest.get(1).cloned().unwrap_or_default(),
        })
    }
}

fn with_group(owner: &str) -> String {
    match owner.contains(':') {
        true => owner.to_owned(),
        false => format!("{owner}:{owner}"),
    }
}

fn ensure_trailing_slash(path: &str) -> String {
    match path.ends_with('/') {
        true => path.to_owned(),
        false => format!("{path}/"),
    }
}

fn split_pattern(pattern: &str) -> String {
    match base_split(pattern.as_bytes()) {
        Some(0) => "/".to_owned(),
        Some(split) => unescape_meta(&pattern[..split]),
        None => ".".to_owned(),
    }
}

fn base_split(pattern: &[u8]) -> Option<usize> {
    let mut split = None;
    let mut index = 0;
    while index < pattern.len() {
        match pattern[index] {
            b'\\' => index += 1,
            b'/' => split = Some(index),
            b'*' | b'?' | b'[' | b'{' => break,
            _ => {}
        }
        index += 1;
    }
    split
}

fn unescape_meta(path: &str) -> String {
    let mut unescaped = String::with_capacity(path.len());
    let mut chars = path.chars().peekable();
    while let Some(current) = chars.next() {
        match chars.next_if(|next| current == '\\' && META.contains(next)) {
            Some(meta) => unescaped.push(meta),
            None => unescaped.push(current),
        }
    }
    unescaped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str], user: &str) -> Result<CopyArgs, String> {
        CopyArgs::parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>(), user)
    }

    fn expect(source: &str, target: &str, owner: &str, permissions: &str) -> Result<CopyArgs, String> {
        Ok(CopyArgs {
            source_path: source.to_owned(),
            target_path: target.to_owned(),
            owner: owner.to_owned(),
            permissions: permissions.to_owned(),
        })
    }

    #[test]
    fn parses_copy_args_like_upstream() {
        let cases: &[(&[&str], &str, Result<CopyArgs, String>)] = &[
            (
                &["/local/path", "/container/path"],
                "user",
                expect("/local/path", "/container/path", "user:user", ""),
            ),
            (
                &["/local/path", "/container/path", "root"],
                "user",
                expect("/local/path", "/container/path", "root:root", ""),
            ),
            (
                &["/local/path", "/container/path", "www-data:www-data"],
                "user",
                expect("/local/path", "/container/path", "www-data:www-data", ""),
            ),
            (
                &["/local/path", "/container/path", "user:staff"],
                "defaultuser",
                expect("/local/path", "/container/path", "user:staff", ""),
            ),
            (
                &["/local/path", "/container/path", ""],
                "ubuntu",
                expect("/local/path", "/container/path", "ubuntu:ubuntu", ""),
            ),
            (
                &["/local/path", "/container/path", "root", "755"],
                "user",
                expect("/local/path", "/container/path", "root:root", "755"),
            ),
            (
                &["/local/path", "/container/path", "www-data:www-data", "644"],
                "user",
                expect("/local/path", "/container/path", "www-data:www-data", "644"),
            ),
            (
                &["/app/*.js", "/dest/"],
                "user",
                expect("/app", "/dest/", "user:user", ""),
            ),
            (
                &["/app/**/*.ts", "/dest/"],
                "user",
                expect("/app", "/dest/", "user:user", ""),
            ),
            (
                &["/app/file?.txt", "/dest/"],
                "user",
                expect("/app", "/dest/", "user:user", ""),
            ),
            (
                &["/app/[abc].txt", "/dest/"],
                "user",
                expect("/app", "/dest/", "user:user", ""),
            ),
            (
                &["/app/src/", "/dest/"],
                "user",
                expect("/app/src", "/dest/", "user:user", ""),
            ),
            (
                &["/app/src", "/dest/"],
                "user",
                expect("/app/src", "/dest/", "user:user", ""),
            ),
            (
                &["./local/path", "container/path"],
                "user",
                expect("./local/path", "container/path", "user:user", ""),
            ),
            (
                &["/src/components/**/*.{ts,tsx}", "/dest/"],
                "developer",
                expect("/src/components", "/dest/", "developer:developer", ""),
            ),
            (
                &["/source/*.go", "/target/", "admin:sudo", "700"],
                "user",
                expect("/source", "/target/", "admin:sudo", "700"),
            ),
            (
                &[],
                "user",
                Err("COPY requires a local path and a container path argument".to_owned()),
            ),
            (
                &["/local/path"],
                "user",
                Err("COPY requires a local path and a container path argument".to_owned()),
            ),
        ];

        for (args, user, expected) in cases {
            assert_eq!(&parse(args, user), expected, "{args:?}");
        }
    }

    #[test]
    fn split_pattern_edges() {
        assert_eq!(split_pattern("/"), "/");
        assert_eq!(split_pattern("*.txt/"), ".");
        assert_eq!(split_pattern("a\\*b/"), "a*b");
        assert_eq!(split_pattern("dir/"), "dir");
        assert_eq!(split_pattern("a\\[b\\]/c/"), "a[b]/c");
        assert_eq!(split_pattern("a\\\\[b]/c/"), ".");
    }
}

#[cfg(all(test, target_os = "linux"))]
mod script_behaviour {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    use super::*;

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Entry {
        File,
        Dir,
        Symlink,
    }

    struct Case {
        name: &'static str,
        files: &'static [(&'static str, Entry)],
        preexisting: &'static [(&'static str, Entry)],
        from: &'static str,
        to: &'static str,
        permissions: &'static str,
        expected: &'static [(&'static str, Entry)],
    }

    const CASES: &[Case] = &[
        Case {
            name: "single_file_root_level",
            files: &[("test.txt", Entry::File)],
            preexisting: &[],
            from: "test.txt",
            to: "dest/file.txt",
            permissions: "",
            expected: &[("dest/file.txt", Entry::File)],
        },
        Case {
            name: "single_file_to_new_nested_directory_trailing_slash",
            files: &[("test.sh", Entry::File)],
            preexisting: &[],
            from: "test.sh",
            to: "a/b/c/",
            permissions: "",
            expected: &[("a/b/c/test.sh", Entry::File)],
        },
        Case {
            name: "single_file_to_preexisting_directory_no_slash",
            files: &[("test.sh", Entry::File)],
            preexisting: &[("dest", Entry::Dir)],
            from: "test.sh",
            to: "dest",
            permissions: "",
            expected: &[("dest/test.sh", Entry::File)],
        },
        Case {
            name: "symlink_to_new_directory_trailing_slash",
            files: &[("link.txt", Entry::Symlink)],
            preexisting: &[],
            from: "link.txt",
            to: "newdir/",
            permissions: "",
            expected: &[("newdir/link.txt", Entry::Symlink)],
        },
        Case {
            name: "merge_directory_into_existing_tree",
            files: &[
                ("app", Entry::Dir),
                ("app/main.js", Entry::File),
                ("app/src", Entry::Dir),
                ("app/src/index.js", Entry::File),
            ],
            preexisting: &[("app", Entry::Dir), ("app/old.js", Entry::File)],
            from: "app/",
            to: "app/",
            permissions: "",
            expected: &[
                ("app/main.js", Entry::File),
                ("app/src/index.js", Entry::File),
                ("app/old.js", Entry::File),
            ],
        },
        Case {
            name: "with_permissions_755",
            files: &[("run.sh", Entry::File)],
            preexisting: &[],
            from: "run.sh",
            to: "bin/run.sh",
            permissions: "755",
            expected: &[("bin/run.sh", Entry::File)],
        },
    ];

    fn create(root: &Path, entries: &[(&str, Entry)]) {
        for (path, entry) in entries {
            let full = root.join(path);
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            match entry {
                Entry::File => std::fs::write(&full, path.as_bytes()).unwrap(),
                Entry::Dir => std::fs::create_dir_all(&full).unwrap(),
                Entry::Symlink => std::os::unix::fs::symlink("missing-target", &full).unwrap(),
            }
        }
    }

    fn kind(path: &Path) -> Option<Entry> {
        let metadata = std::fs::symlink_metadata(path).ok()?;
        match (metadata.is_symlink(), metadata.is_dir()) {
            (true, _) => Some(Entry::Symlink),
            (false, true) => Some(Entry::Dir),
            (false, false) => Some(Entry::File),
        }
    }

    fn run(case: &Case) {
        let dir = tempfile::tempdir().unwrap();
        let unpack = dir.path().join("unpack");
        let target = dir.path().join("target");
        std::fs::create_dir_all(&unpack).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        create(&unpack, case.files);
        create(&target, case.preexisting);
        let target_path = format!("{}/{}", target.display(), case.to);
        let owner = format!("{}:{}", nix::unistd::getuid(), nix::unistd::getgid());
        let source_path = join_path(&[&unpack.display().to_string(), case.from]);
        let workdir = target.display().to_string();
        let script = render(
            COPY_SCRIPT,
            &[
                ("TargetPath", &target_path),
                ("SourcePath", &source_path),
                ("Owner", &owner),
                ("Permissions", case.permissions),
                ("Workdir", &workdir),
                ("User", ""),
            ],
        )
        .unwrap();

        let output = std::process::Command::new("/bin/bash")
            .arg("-c")
            .arg(&script)
            .output()
            .unwrap();

        assert!(output.status.success(), "{}: {output:?}", case.name);
        for (path, entry) in case.expected {
            assert_eq!(kind(&target.join(path)), Some(*entry), "{}: {path}", case.name);
        }
        if !case.permissions.is_empty() {
            let mode = std::fs::metadata(target.join(case.expected[0].0))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(format!("{mode:o}"), case.permissions, "{}", case.name);
        }
    }

    #[test]
    fn the_copy_script_moves_entries_with_docker_copy_semantics() {
        CASES.iter().for_each(run);
    }
}
