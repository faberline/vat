//! `vat image build`: a Dockerfile-subset ("Vatfile") builder for
//! `darwin/arm64` images.
//!
//! Supported instructions: `FROM scratch` / `FROM <local darwin image>`,
//! `WORKDIR`, `ENV`, `COPY src... dest`, `RUN`, `CMD`, `ENTRYPOINT`, `LABEL`,
//! `EXPOSE` (recorded only). Everything else is rejected with a clear error.
//!
//! Each step runs against a fixed-length build root. `RUN` executes
//! `/bin/sh -c` **on the host** with cwd = root + WORKDIR, env = image env +
//! `VAT_ROOT`, under the same seatbelt profile as containers (writes only in
//! the root, whose `tmp/` is `TMPDIR`). Each filesystem-changing step becomes
//! a gzip tar layer from a before/after scan; build-root bytes in committed
//! content are replaced by the relocation placeholder.

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use super::container::{resolve_argv, runtime_env, user_cache_dir};
use super::layer::{self, ScanOptions};
use super::oci::{
    self, ContainerConfig, Descriptor, History, ImageConfig, Manifest, Reference, RootFs,
};
use super::root::{self, RootKind};
use super::store::ImageStore;
use crate::sandbox::seatbelt;
use crate::spec::EgressPolicy;

/// Exec-form (`["a","b"]`) or shell-form command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Form {
    Exec(Vec<String>),
    Shell(String),
}

impl Form {
    fn argv(&self) -> Vec<String> {
        match self {
            Form::Exec(argv) => argv.clone(),
            Form::Shell(cmd) => vec!["/bin/sh".into(), "-c".into(), cmd.clone()],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Instruction {
    From(String),
    Workdir(String),
    Env(Vec<(String, String)>),
    Copy { srcs: Vec<String>, dest: String },
    Run(Form),
    Cmd(Form),
    Entrypoint(Form),
    Label(Vec<(String, String)>),
    Expose(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub line: usize,
    pub text: String,
    pub instruction: Instruction,
}

const SUPPORTED: &str = "FROM, WORKDIR, ENV, COPY, RUN, CMD, ENTRYPOINT, LABEL, EXPOSE";

/// Split shell-style words (quotes and backslash escapes; no expansion).
pub fn shell_words(input: &str) -> Result<Vec<String>> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(ch) => current.push(ch),
                        None => bail!("unterminated single quote in {input:?}"),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            Some(ch @ ('"' | '\\' | '$')) => current.push(ch),
                            Some(ch) => {
                                current.push('\\');
                                current.push(ch);
                            }
                            None => bail!("unterminated double quote in {input:?}"),
                        },
                        Some(ch) => current.push(ch),
                        None => bail!("unterminated double quote in {input:?}"),
                    }
                }
            }
            '\\' => {
                in_word = true;
                if let Some(ch) = chars.next() {
                    current.push(ch);
                }
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                current.push(c);
            }
        }
    }
    if in_word {
        words.push(current);
    }
    Ok(words)
}

/// Expand `$VAR` / `${VAR}` from `env`, leaving `$VAT_ROOT` literal (it is
/// resolved per root at run time).
pub fn expand(value: &str, env: &BTreeMap<String, String>) -> String {
    let mut out = String::new();
    let bytes: Vec<char> = value.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == '$' && i + 1 < bytes.len() {
            let (name, end, braced) = if bytes[i + 1] == '{' {
                match bytes[i + 2..].iter().position(|c| *c == '}') {
                    Some(close) => (bytes[i + 2..i + 2 + close].iter().collect::<String>(), i + 3 + close, true),
                    None => {
                        out.push('$');
                        i += 1;
                        continue;
                    }
                }
            } else {
                let len = bytes[i + 1..]
                    .iter()
                    .take_while(|c| c.is_ascii_alphanumeric() || **c == '_')
                    .count();
                (bytes[i + 1..i + 1 + len].iter().collect::<String>(), i + 1 + len, false)
            };
            if name.is_empty() {
                out.push('$');
                i += 1;
                continue;
            }
            if name == "VAT_ROOT" {
                if braced {
                    out.push_str("${VAT_ROOT}");
                } else {
                    out.push_str("$VAT_ROOT");
                }
            } else if let Some(v) = env.get(&name) {
                out.push_str(v);
            }
            i = end;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    out
}

fn parse_form(args: &str) -> Result<Form> {
    let trimmed = args.trim();
    if trimmed.starts_with('[') {
        if let Ok(argv) = serde_json::from_str::<Vec<String>>(trimmed) {
            if argv.is_empty() {
                bail!("empty exec-form command");
            }
            return Ok(Form::Exec(argv));
        }
    }
    if trimmed.is_empty() {
        bail!("missing command");
    }
    Ok(Form::Shell(trimmed.to_string()))
}

fn parse_pairs(keyword: &str, args: &str) -> Result<Vec<(String, String)>> {
    let words = shell_words(args)?;
    if words.is_empty() {
        bail!("{keyword} needs at least one KEY=VALUE");
    }
    if !words[0].contains('=') {
        if keyword == "ENV" {
            // Legacy `ENV KEY value with spaces`.
            let mut parts = args.trim().splitn(2, char::is_whitespace);
            let key = parts.next().unwrap_or_default().to_string();
            let value = parts.next().unwrap_or_default().trim().to_string();
            let value = shell_words(&value)?.join(" ");
            return Ok(vec![(key, value)]);
        }
        bail!("{keyword} expects KEY=VALUE pairs");
    }
    words
        .into_iter()
        .map(|w| match w.split_once('=') {
            Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
            _ => bail!("{keyword} expects KEY=VALUE pairs, got {w:?}"),
        })
        .collect()
}

/// Parse a Vatfile.
pub fn parse(text: &str) -> Result<Vec<Step>> {
    let mut steps = Vec::new();
    let mut logical = String::new();
    let mut start_line = 0usize;
    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let trimmed = raw.trim();
        if trimmed.starts_with('#') || (trimmed.is_empty() && !logical.is_empty()) {
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if logical.is_empty() {
            start_line = line_no;
        }
        let body = raw.trim_end();
        if let Some(stripped) = body.strip_suffix('\\') {
            logical.push_str(stripped);
            logical.push(' ');
            continue;
        }
        logical.push_str(body);
        let full = std::mem::take(&mut logical);
        steps.push(parse_instruction(start_line, full.trim())?);
    }
    if !logical.trim().is_empty() {
        steps.push(parse_instruction(start_line, logical.trim())?);
    }
    match steps.first() {
        None => bail!("the Vatfile has no instructions"),
        Some(step) if !matches!(step.instruction, Instruction::From(_)) => {
            bail!("line {}: the first instruction must be FROM", step.line)
        }
        _ => {}
    }
    if steps.iter().skip(1).any(|s| matches!(s.instruction, Instruction::From(_))) {
        bail!("multi-stage builds (more than one FROM) are not supported by vat image build");
    }
    Ok(steps)
}

fn parse_instruction(line: usize, text: &str) -> Result<Step> {
    let (keyword, args) = match text.split_once(char::is_whitespace) {
        Some((k, a)) => (k.to_ascii_uppercase(), a.trim().to_string()),
        None => (text.to_ascii_uppercase(), String::new()),
    };
    let ctx = || format!("line {line}: {keyword}");
    let instruction = match keyword.as_str() {
        "FROM" => {
            let mut words = shell_words(&args).with_context(ctx)?;
            if let Some(first) = words.first() {
                if let Some(platform) = first.strip_prefix("--platform=") {
                    if platform != "darwin/arm64" {
                        bail!("line {line}: FROM --platform={platform} is not supported (native images are darwin/arm64)");
                    }
                    words.remove(0);
                }
            }
            match words.as_slice() {
                [image] => Instruction::From(image.clone()),
                [_, as_kw, _] if as_kw.eq_ignore_ascii_case("as") => {
                    bail!("line {line}: FROM ... AS (multi-stage builds) is not supported")
                }
                _ => bail!("line {line}: FROM expects exactly one image (or `scratch`)"),
            }
        }
        "WORKDIR" => {
            let words = shell_words(&args).with_context(ctx)?;
            match words.as_slice() {
                [dir] => Instruction::Workdir(dir.clone()),
                _ => bail!("line {line}: WORKDIR expects one path"),
            }
        }
        "ENV" => Instruction::Env(parse_pairs("ENV", &args).with_context(ctx)?),
        "LABEL" => Instruction::Label(parse_pairs("LABEL", &args).with_context(ctx)?),
        "EXPOSE" => {
            let ports = shell_words(&args).with_context(ctx)?;
            if ports.is_empty() {
                bail!("line {line}: EXPOSE needs at least one port");
            }
            Instruction::Expose(ports)
        }
        "COPY" => {
            let words = if args.trim_start().starts_with('[') {
                serde_json::from_str::<Vec<String>>(args.trim())
                    .with_context(|| format!("line {line}: COPY exec form must be a JSON string array"))?
            } else {
                shell_words(&args).with_context(ctx)?
            };
            if let Some(flag) = words.iter().find(|w| w.starts_with("--")) {
                bail!("line {line}: COPY flag {flag} is not supported (no --from/--chown/--chmod)");
            }
            if words.len() < 2 {
                bail!("line {line}: COPY expects at least one source and a destination");
            }
            let mut srcs = words;
            let dest = srcs.pop().expect("len >= 2");
            Instruction::Copy { srcs, dest }
        }
        "RUN" => {
            if args.trim_start().starts_with("--") {
                bail!("line {line}: RUN flags (--mount, --network, …) are not supported");
            }
            Instruction::Run(parse_form(&args).with_context(ctx)?)
        }
        "CMD" => Instruction::Cmd(parse_form(&args).with_context(ctx)?),
        "ENTRYPOINT" => Instruction::Entrypoint(parse_form(&args).with_context(ctx)?),
        other => bail!(
            "line {line}: unsupported Vatfile instruction {other}; vat image build supports {SUPPORTED}"
        ),
    };
    Ok(Step { line, text: text.to_string(), instruction })
}

/// `vat image build` options.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub tag: String,
    pub file: Option<PathBuf>,
    pub context: PathBuf,
}

/// What a build produced.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BuildOutcome {
    pub reference: String,
    pub digest: String,
    pub config_digest: String,
    pub layers: usize,
    pub new_layers: usize,
    pub relocations: usize,
    pub steps: usize,
}

/// Removes the build root on every exit path.
struct RootGuard(PathBuf);

impl Drop for RootGuard {
    fn drop(&mut self) {
        let _ = super::remove_tree(&self.0);
    }
}

fn env_map(env: &[String]) -> BTreeMap<String, String> {
    env.iter()
        .map(|e| match e.split_once('=') {
            Some((k, v)) => (k.to_string(), v.to_string()),
            None => (e.clone(), String::new()),
        })
        .collect()
}

fn set_env(config: &mut ContainerConfig, key: &str, value: &str) {
    let entry = format!("{key}={value}");
    match config.env.iter_mut().find(|e| e.split_once('=').map(|(k, _)| k) == Some(key)) {
        Some(existing) => *existing = entry,
        None => config.env.push(entry),
    }
}

fn in_image_dir(config: &ContainerConfig, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        let base = config.working_dir.clone().unwrap_or_else(|| "/".into());
        format!("{}/{}", base.trim_end_matches('/'), path)
    }
}

fn root_join(root: &Path, in_image: &str) -> Result<PathBuf> {
    let trimmed = in_image.trim_start_matches('/');
    if trimmed.is_empty() {
        return Ok(root.to_path_buf());
    }
    Ok(root.join(root::sanitize_rel(trimmed)?))
}

/// Copy `src` into `dst`, merging directories, preserving modes, symlinks,
/// and mtimes.
fn copy_into(src: &Path, dst: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.is_dir() {
        match std::fs::symlink_metadata(dst) {
            Ok(existing) if existing.is_dir() => {}
            Ok(_) => {
                std::fs::remove_file(dst)?;
                std::fs::create_dir_all(dst)?;
            }
            Err(_) => std::fs::create_dir_all(dst)?,
        }
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_into(&entry.path(), &dst.join(entry.file_name()))?;
        }
        std::fs::set_permissions(dst, std::fs::Permissions::from_mode(meta.permissions().mode()))?;
        let mtime = filetime::FileTime::from_last_modification_time(&meta);
        let _ = filetime::set_file_times(dst, mtime, mtime);
        return Ok(());
    }
    if let Ok(existing) = std::fs::symlink_metadata(dst) {
        if existing.is_dir() {
            super::remove_tree(dst)?;
        } else {
            std::fs::remove_file(dst)?;
        }
    }
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mtime = filetime::FileTime::from_last_modification_time(&meta);
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
        let _ = filetime::set_symlink_file_times(dst, mtime, mtime);
    } else if meta.is_file() {
        std::fs::copy(src, dst).with_context(|| format!("copy {}", src.display()))?;
        let _ = filetime::set_file_times(dst, mtime, mtime);
    }
    Ok(())
}

fn copy_step(context: &Path, root: &Path, config: &ContainerConfig, srcs: &[String], dest: &str) -> Result<()> {
    let dest_in_image = in_image_dir(config, dest);
    let dest_path = root_join(root, &dest_in_image)?;
    let dest_is_dir = dest.ends_with('/') || srcs.len() > 1 || dest_path.is_dir();
    for src in srcs {
        if src.contains(['*', '?', '[']) {
            bail!("COPY source {src:?}: wildcards are not supported by vat image build");
        }
        let candidate = context.join(src.trim_start_matches('/'));
        let resolved = std::fs::canonicalize(&candidate)
            .with_context(|| format!("COPY source {src:?} not found in the build context"))?;
        if !resolved.starts_with(context) {
            bail!("COPY source {src:?} resolves outside the build context");
        }
        if resolved.is_dir() {
            std::fs::create_dir_all(&dest_path)?;
            for entry in std::fs::read_dir(&resolved)? {
                let entry = entry?;
                copy_into(&entry.path(), &dest_path.join(entry.file_name()))?;
            }
        } else if dest_is_dir {
            std::fs::create_dir_all(&dest_path)?;
            let name = resolved.file_name().context("COPY source has no file name")?;
            copy_into(&resolved, &dest_path.join(name))?;
        } else {
            copy_into(&resolved, &dest_path)?;
        }
    }
    Ok(())
}

fn run_step(root: &Path, config: &ContainerConfig, form: &Form) -> Result<()> {
    let env = runtime_env(root, &config.env, &[])?;
    let workdir = match config.working_dir.as_deref() {
        Some(dir) => root_join(root, dir)?,
        None => root.to_path_buf(),
    };
    std::fs::create_dir_all(&workdir)?;
    if let Some((_, home)) = env.iter().find(|(k, _)| k == "HOME") {
        if Path::new(home).starts_with(root) {
            std::fs::create_dir_all(home)?;
        }
    }
    let exec_config = ContainerConfig { cmd: Some(form.argv()), ..Default::default() };
    let (_, argv) = resolve_argv(root, &exec_config, &[])?;
    let writable: Vec<PathBuf> = user_cache_dir().into_iter().collect();
    let profile = seatbelt::native_container_profile(root, &writable, EgressPolicy::Open);
    let stderr_for_child = {
        use std::os::fd::AsFd;
        std::io::stderr().as_fd().try_clone_to_owned()?
    };
    let status = Command::new("/usr/bin/sandbox-exec")
        .arg("-p")
        .arg(&profile)
        .args(&argv)
        .env_clear()
        .envs(env)
        .current_dir(&workdir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stderr_for_child))
        .status()
        .context("spawn sandbox-exec for RUN")?;
    if !status.success() {
        bail!("RUN failed ({status})");
    }
    Ok(())
}

/// Build an image from a Vatfile.
pub fn build(opts: &BuildOptions) -> Result<BuildOutcome> {
    let reference = Reference::parse(&opts.tag).context("invalid -t")?;
    let context = std::fs::canonicalize(&opts.context)
        .with_context(|| format!("build context {} does not exist", opts.context.display()))?;
    let file = match &opts.file {
        Some(file) => file.clone(),
        None => {
            let vatfile = context.join("Vatfile");
            if vatfile.is_file() {
                vatfile
            } else {
                context.join("Dockerfile")
            }
        }
    };
    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("read {}", file.display()))?;
    let steps = parse(&text).with_context(|| format!("parse {}", file.display()))?;
    let store = ImageStore::open()?;

    let Instruction::From(from) = &steps[0].instruction else { unreachable!("checked by parse") };
    let base = if from == "scratch" {
        None
    } else {
        Some(store.resolve(from).with_context(|| {
            format!("FROM {from}: not a local darwin/arm64 image (pull or build it first)")
        })?)
    };

    let roots = super::roots_base()?;
    let (_, build_root) = root::allocate(&roots, RootKind::Build)?;
    let _guard = RootGuard(build_root.clone());
    match &base {
        Some(image) => {
            let snapshot = store.snapshot(image)?;
            super::clone_tree(&snapshot, &build_root)?;
            root::relocate(
                &build_root,
                &image.relocations()?,
                root::placeholder().as_bytes(),
                build_root.as_os_str().as_encoded_bytes(),
            )?;
        }
        None => std::fs::create_dir_all(&build_root)?,
    }
    std::fs::set_permissions(&build_root, std::fs::Permissions::from_mode(0o755))?;
    std::fs::create_dir_all(build_root.join("tmp"))?;

    let mut config = base.as_ref().map(|b| b.config.config.clone()).unwrap_or_default();
    let mut layers: Vec<Descriptor> = base.as_ref().map(|b| b.manifest.layers.clone()).unwrap_or_default();
    let mut diff_ids: Vec<String> = base.as_ref().map(|b| b.config.rootfs.diff_ids.clone()).unwrap_or_default();
    let mut history: Vec<History> = base.as_ref().map(|b| b.config.history.clone()).unwrap_or_default();
    let base_layers = layers.len();

    let mut scan_opts = ScanOptions::native_default();
    scan_opts.exclude_contents.push("root/Library/Caches".into());
    let mut before = layer::scan_tree(&build_root, &scan_opts)?;
    let placeholder = root::placeholder();
    let build_root_bytes = build_root.as_os_str().as_encoded_bytes().to_vec();
    let total = steps.len();
    let mut relocations = 0usize;

    for (index, step) in steps.iter().enumerate() {
        eprintln!("STEP {}/{}: {}", index + 1, total, step.text);
        let env = env_map(&config.env);
        let mut fs_step = false;
        let result: Result<()> = (|| {
            match &step.instruction {
                Instruction::From(_) => {}
                Instruction::Env(pairs) => {
                    for (k, v) in pairs {
                        if k == "VAT_ROOT" {
                            bail!("ENV VAT_ROOT is reserved");
                        }
                        let value = expand(v, &env_map(&config.env));
                        set_env(&mut config, k, &value);
                    }
                }
                Instruction::Label(pairs) => {
                    for (k, v) in pairs {
                        config.labels.insert(k.clone(), expand(v, &env));
                    }
                }
                Instruction::Expose(ports) => {
                    for port in ports {
                        let port = expand(port, &env);
                        let key = if port.contains('/') { port } else { format!("{port}/tcp") };
                        config.exposed_ports.insert(key, serde_json::json!({}));
                    }
                }
                Instruction::Cmd(form) => config.cmd = Some(form.argv()),
                Instruction::Entrypoint(form) => {
                    config.entrypoint = Some(form.argv());
                    // Docker semantics: ENTRYPOINT resets an inherited CMD.
                    config.cmd = None;
                }
                Instruction::Workdir(dir) => {
                    let dir = in_image_dir(&config, &expand(dir, &env));
                    std::fs::create_dir_all(root_join(&build_root, &dir)?)?;
                    config.working_dir = Some(dir);
                    fs_step = true;
                }
                Instruction::Copy { srcs, dest } => {
                    let srcs: Vec<String> = srcs.iter().map(|s| expand(s, &env)).collect();
                    copy_step(&context, &build_root, &config, &srcs, &expand(dest, &env))?;
                    fs_step = true;
                }
                Instruction::Run(form) => {
                    run_step(&build_root, &config, form)?;
                    fs_step = true;
                }
            }
            Ok(())
        })();
        result.with_context(|| format!("line {}: {}", step.line, step.text))?;

        let mut entry = History {
            created: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            created_by: Some(format!("vat image build: {}", step.text)),
            comment: None,
            empty_layer: Some(true),
        };
        if fs_step {
            let after = layer::scan_tree(&build_root, &scan_opts)?;
            let changes = layer::diff(&before, &after);
            if !changes.is_empty() {
                let tmp = store.tmp_dir()?.join(format!("layer-{}", super::random_hex(8)));
                let blob = layer::write_layer(
                    &build_root,
                    &changes,
                    &after,
                    Some((&build_root_bytes, placeholder.as_bytes())),
                    &tmp,
                )?;
                store.adopt(&tmp, &blob.digest)?;
                let mut desc = Descriptor::new(oci::MT_OCI_LAYER_GZIP, blob.digest.clone(), blob.size);
                if !blob.relocations.is_empty() {
                    desc.annotations.insert(
                        oci::ANN_RELOCATIONS.into(),
                        serde_json::to_string(&blob.relocations)?,
                    );
                }
                relocations += blob.relocations.len();
                eprintln!(
                    " → layer {} ({} entries, {} relocations)",
                    &blob.digest[..19],
                    blob.entries,
                    blob.relocations.len()
                );
                layers.push(desc);
                diff_ids.push(blob.diff_id);
                entry.empty_layer = None;
            }
            before = after;
        }
        if !matches!(step.instruction, Instruction::From(_)) {
            history.push(entry);
        }
    }

    let image_config = ImageConfig {
        created: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
        architecture: oci::ARCH.into(),
        os: oci::OS.into(),
        config,
        rootfs: RootFs { kind: "layers".into(), diff_ids },
        history,
    };
    let config_bytes = serde_json::to_vec(&image_config)?;
    let config_digest = store.put_blob(&config_bytes)?;
    let mut annotations = BTreeMap::new();
    annotations.insert(oci::ANN_ROOT_LENGTH.into(), root::ROOT_LEN.to_string());
    if let Some(created) = &image_config.created {
        annotations.insert(oci::ANN_CREATED.into(), created.clone());
    }
    let manifest = Manifest {
        schema_version: 2,
        media_type: Some(oci::MT_OCI_MANIFEST.into()),
        config: Descriptor::new(oci::MT_OCI_CONFIG, config_digest.clone(), config_bytes.len() as u64),
        layers,
        annotations,
    };
    let manifest_bytes = serde_json::to_vec(&manifest)?;
    let digest = store.put_blob(&manifest_bytes)?;
    let name = store.set_ref(&reference.local_name(), &digest)?;
    Ok(BuildOutcome {
        reference: name,
        digest,
        config_digest,
        layers: manifest.layers.len(),
        new_layers: manifest.layers.len() - base_layers,
        relocations,
        steps: total,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_instructions() {
        let text = r#"
# comment
FROM scratch
WORKDIR /app
ENV A=1 B="two words"
ENV LEGACY some value
COPY a.txt b/ /dest/
RUN echo hi && \
    echo there
CMD ["python3", "-m", "http.server"]
ENTRYPOINT /app/run.sh
LABEL org.example="x y"
EXPOSE 8000 9000/udp
"#;
        let steps = parse(text).unwrap();
        assert_eq!(steps[0].instruction, Instruction::From("scratch".into()));
        assert_eq!(
            steps[2].instruction,
            Instruction::Env(vec![("A".into(), "1".into()), ("B".into(), "two words".into())])
        );
        assert_eq!(steps[3].instruction, Instruction::Env(vec![("LEGACY".into(), "some value".into())]));
        assert_eq!(
            steps[4].instruction,
            Instruction::Copy { srcs: vec!["a.txt".into(), "b/".into()], dest: "/dest/".into() }
        );
        assert_eq!(steps[5].instruction, Instruction::Run(Form::Shell("echo hi &&      echo there".into())));
        assert_eq!(steps[5].line, 8);
        assert_eq!(
            steps[6].instruction,
            Instruction::Cmd(Form::Exec(vec!["python3".into(), "-m".into(), "http.server".into()]))
        );
        assert_eq!(steps[7].instruction, Instruction::Entrypoint(Form::Shell("/app/run.sh".into())));
        assert_eq!(steps[9].instruction, Instruction::Expose(vec!["8000".into(), "9000/udp".into()]));
    }

    #[test]
    fn rejects_unsupported_instructions_clearly() {
        let err = parse("FROM scratch\nADD x /x\n").unwrap_err().to_string();
        assert!(err.contains("unsupported Vatfile instruction ADD"), "{err}");
        assert!(err.contains("line 2"));
        assert!(parse("RUN true\n").unwrap_err().to_string().contains("first instruction must be FROM"));
        assert!(parse("FROM a AS b\n").is_err());
        assert!(parse("FROM scratch\nFROM scratch\n").is_err());
        assert!(parse("FROM scratch\nCOPY --from=a x y\n").is_err());
        assert!(parse("FROM scratch\nUSER nobody\n").is_err());
        assert!(parse("FROM --platform=linux/amd64 x\n").is_err());
        assert!(parse("FROM --platform=darwin/arm64 x\n").is_ok());
    }

    #[test]
    fn expansion_keeps_vat_root_literal() {
        let env: BTreeMap<String, String> = [("PATH".to_string(), "/a".to_string())].into_iter().collect();
        assert_eq!(expand("$VAT_ROOT/bin:$PATH", &env), "$VAT_ROOT/bin:/a");
        assert_eq!(expand("${VAT_ROOT}/x ${PATH}y $MISSING.", &env), "${VAT_ROOT}/x /ay .");
        assert_eq!(expand("cost $", &env), "cost $");
    }

    #[test]
    fn shell_words_handles_quotes() {
        assert_eq!(shell_words(r#"a "b c" 'd e' f\ g"#).unwrap(), vec!["a", "b c", "d e", "f g"]);
        assert!(shell_words("\"open").is_err());
    }
}
