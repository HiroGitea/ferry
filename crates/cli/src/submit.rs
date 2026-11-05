use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::Args;
use mlq_client::Client;
use mlq_core::Job;
use mlq_core::api::SubmitJobRequest;
use mlq_core::resource::ResourceOverrides;

#[derive(Args, Debug, Clone, Default)]
pub struct SubmitArgs {
    /// Job name
    #[arg(short = 'J', long)]
    pub name: Option<String>,
    /// CPU cores [default: 2, or the task's default]
    #[arg(short = 'c', long)]
    pub cpus: Option<f64>,
    /// Memory limit, e.g. 16G [default: 8G, or the task's default]
    #[arg(short = 'm', long)]
    pub mem: Option<String>,
    /// Number of GPUs [default: 0, or the task's default]
    #[arg(short = 'g', long)]
    pub gpus: Option<u32>,
    /// Run as the next step of this task (environment saved between steps)
    #[arg(short = 'T', long = "task")]
    pub task: Option<i64>,
    /// Run on this node only (see `mlq node list`)
    #[arg(short = 'N', long)]
    pub node: Option<String>,
    /// Container image
    #[arg(short = 'i', long)]
    pub image: Option<String>,
    /// Time limit (2:00:00, 90 = minutes, 2h30m)
    #[arg(short = 't', long)]
    pub time: Option<String>,
    /// Priority (higher runs first)
    #[arg(short = 'p', long, default_value = "0")]
    pub priority: i32,
    /// Start only after these jobs succeed
    #[arg(long = "after", value_delimiter = ',')]
    pub depends_on: Vec<i64>,
    /// Job array, e.g. 0-3 or 0,2,5 (sets MLQ_ARRAY_INDEX)
    #[arg(long)]
    pub array: Option<String>,
    /// Environment variable KEY=VALUE (repeatable)
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,
    /// Upload a local directory (or file) into /work before the job starts
    #[arg(long)]
    pub upload: Vec<PathBuf>,
    /// Submit in held state
    #[arg(long)]
    pub hold: bool,
    /// Validate and show the scheduling estimate without submitting
    #[arg(long)]
    pub dry_run: bool,
    /// Command (after `--`), or a single script path whose `#MLQ` lines carry options
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub command: Vec<String>,
}

/// Parse `#MLQ --gpus 1 --mem 16G` directive lines from a script.
pub fn parse_directives(script: &str) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for line in script.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("#MLQ ") {
            out.extend(shell_words(rest)?);
        }
    }
    Ok(out)
}

fn shell_words(s: &str) -> anyhow::Result<Vec<String>> {
    // Minimal splitter supporting double/single quotes.
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for ch in s.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => cur.push(c),
            (None, '"') | (None, '\'') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if quote.is_some() {
        bail!("unterminated quote in directive: {s}");
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

fn tar_gz(paths: &[PathBuf]) -> anyhow::Result<Vec<u8>> {
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    let mut ar = tar::Builder::new(enc);
    ar.follow_symlinks(true);
    for p in paths {
        let name = p.file_name().context("upload path has no file name")?;
        if p.is_dir() {
            ar.append_dir_all(name, p)
                .with_context(|| format!("adding {}", p.display()))?;
        } else {
            ar.append_path_with_name(p, name)
                .with_context(|| format!("adding {}", p.display()))?;
        }
    }
    Ok(ar.into_inner()?.finish()?)
}

pub fn build_request(args: &SubmitArgs) -> anyhow::Result<(SubmitJobRequest, Vec<PathBuf>)> {
    let mut args = args.clone();
    let mut uploads = args.upload.clone();
    // Script mode: `mlq submit train.sh`
    let first = args.command.first().cloned().unwrap_or_default();
    let is_script = (first.ends_with(".sh") || first.ends_with(".py"))
        && std::path::Path::new(&first).is_file();
    if is_script {
        let path = PathBuf::from(&first);
        let extra_args: Vec<String> = args.command[1..].to_vec();
        let text = std::fs::read_to_string(&path)?;
        let directives = parse_directives(&text)?;
        if !directives.is_empty() {
            let mut argv = vec!["mlq-directives".to_string()];
            argv.extend(directives);
            #[derive(clap::Parser)]
            struct Directives {
                #[command(flatten)]
                args: SubmitArgs,
            }
            let parsed = <Directives as clap::Parser>::try_parse_from(&argv)
                .context("parsing #MLQ directives")?
                .args;
            // Directives provide defaults; explicit CLI flags are not distinguishable from defaults here,
            // so directives win only for fields left at their default on the command line.
            args.cpus = args.cpus.or(parsed.cpus);
            args.mem = args.mem.or(parsed.mem);
            args.gpus = args.gpus.or(parsed.gpus);
            args.task = args.task.or(parsed.task);
            args.node = args.node.or(parsed.node);
            if args.image.is_none() {
                args.image = parsed.image;
            }
            if args.time.is_none() {
                args.time = parsed.time;
            }
            if args.priority == 0 {
                args.priority = parsed.priority;
            }
            if args.name.is_none() {
                args.name = parsed.name;
            }
            if args.array.is_none() {
                args.array = parsed.array;
            }
            args.env.extend(parsed.env);
            uploads.extend(parsed.upload);
        }
        let fname = path.file_name().unwrap().to_string_lossy().to_string();
        uploads.push(path.clone());
        if args.name.is_none() {
            args.name = Some(fname.clone());
        }
        let interp = if fname.ends_with(".py") {
            "python"
        } else {
            "bash"
        };
        args.command = vec![interp.into(), fname];
        args.command.extend(extra_args);
    }
    if args.command.is_empty() {
        bail!("no command given (use `mlq submit [opts] -- cmd args...`)");
    }
    let mut env = BTreeMap::new();
    for kv in &args.env {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("bad --env '{kv}', expected KEY=VALUE"))?;
        env.insert(k.to_string(), v.to_string());
    }
    let req = SubmitJobRequest {
        name: args.name.clone(),
        image: args.image.clone(),
        command: args.command.clone(),
        env,
        resources: ResourceOverrides {
            cpus: args.cpus,
            mem_bytes: args.mem.as_deref().map(mlq_core::parse_bytes).transpose()?,
            gpus: args.gpus,
        },
        priority: args.priority,
        depends_on: args.depends_on.clone(),
        array: args.array.clone(),
        time_limit_secs: args
            .time
            .as_deref()
            .map(mlq_core::parse_duration_secs)
            .transpose()?,
        hold: args.hold || !uploads.is_empty(),
        files: BTreeMap::new(),
        dry_run: args.dry_run,
        task_id: args.task,
        node: args.node.clone(),
    };
    Ok((req, uploads))
}

pub async fn submit(c: &Client, args: &SubmitArgs, quiet: bool) -> anyhow::Result<Vec<Job>> {
    let (req, uploads) = build_request(args)?;
    let resp = c.submit(&req).await?;
    if req.dry_run {
        println!("dry run: {}", resp.estimate);
        return Ok(vec![]);
    }
    let mut jobs = resp.jobs;
    if !uploads.is_empty() {
        let data = tar_gz(&uploads)?;
        for j in &mut jobs {
            c.upload_tar_gz(j.id, data.clone())
                .await
                .with_context(|| format!("uploading files for job {}", j.id))?;
            if !args.hold {
                *j = c.release(j.id).await?;
            }
        }
    }
    if !quiet {
        for j in &jobs {
            println!("submitted job {} ({}) — {}", j.id, j.name, resp.estimate);
        }
    } else {
        for j in &jobs {
            eprintln!("job {} submitted — {}", j.id, resp.estimate);
        }
    }
    Ok(jobs)
}
