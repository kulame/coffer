//! Coffer CLI — command-line interface for MicroVM sandbox management.
//!
//! Provides fast sandbox lifecycle commands for local testing and development.

use std::collections::HashMap;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use coffer_core::{Runtime, RuntimeConfig, TemplateManager};

// ===================================================================
// CLI definition
// ===================================================================

#[derive(Parser)]
#[command(name = "coffer-cli")]
#[command(about = "Coffer CLI — fast MicroVM sandbox management for testing")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Manage VM templates (build, list, inspect, remove)
    #[command(subcommand)]
    Template(TemplateCommand),

    /// Run a command inside a sandbox (acquire → exec → release)
    Run(RunArgs),

    /// Open an interactive shell inside a sandbox
    Shell(ShellArgs),

    /// Show warm pool status
    PoolStatus,

    /// Check system readiness for running Coffer
    Check,

    /// Show detailed version information (includes git commit and recent log)
    Version,
}

#[derive(Subcommand)]
enum TemplateCommand {
    /// Build a template from an OCI image
    Build {
        /// OCI image reference (e.g. docker.io/library/alpine:latest)
        image: String,
        /// Template name
        #[arg(long, short)]
        name: String,
        /// Custom kernel boot arguments
        #[arg(long)]
        kernel_args: Option<String>,
    },
    /// List all templates
    List,
    /// Show template details
    Info {
        /// Template ID
        id: String,
    },
    /// Remove a template
    Rm {
        /// Template ID
        id: String,
        /// Skip confirmation
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Parser, Debug, Clone)]
struct RunArgs {
    /// Template ID to use
    #[arg(long, short)]
    template: String,

    /// Environment variables (KEY=VALUE)
    #[arg(long, short, value_parser = parse_key_val)]
    env: Vec<(String, String)>,

    /// Timeout in milliseconds
    #[arg(long, default_value = "30000")]
    timeout: u64,

    /// Upload files before running (LOCAL:REMOTE)
    #[arg(long, value_name = "LOCAL:REMOTE")]
    upload: Vec<String>,

    /// Download files after running (REMOTE:LOCAL)
    #[arg(long, value_name = "REMOTE:LOCAL")]
    download: Vec<String>,

    /// Output results as JSON
    #[arg(long)]
    json: bool,

    /// Command and arguments to execute (default: /bin/sh)
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    cmd: Vec<String>,
}

#[derive(Parser, Debug, Clone)]
struct ShellArgs {
    /// Template ID to use
    #[arg(long, short)]
    template: String,

    /// Shell to run (default: /bin/sh)
    #[arg(long, short, default_value = "/bin/sh")]
    shell: String,

    /// Working directory inside the sandbox
    #[arg(long, short)]
    working_dir: Option<String>,

    /// Environment variables (KEY=VALUE)
    #[arg(long, short, value_parser = parse_key_val)]
    env: Vec<(String, String)>,
}

// ===================================================================
// Entry point
// ===================================================================

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // If the binary has cap_net_admin file capabilities, raise it into the
    // ambient set so that child processes (ip, iptables) inherit it.
    coffer_core::ensure_cap_net_admin_ambient();

    tracing_subscriber::fmt::init();

    let cli = Cli::parse();
    match run_cli(cli).await {
        Ok(code) => std::process::ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("Error: {:#}", e);
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run_cli(cli: Cli) -> Result<i32> {
    match cli.command {
        Commands::Template(cmd) => match cmd {
            TemplateCommand::Build {
                image,
                name,
                kernel_args,
            } => {
                cmd_template_build(image, name, kernel_args).await?;
                Ok(0)
            }
            TemplateCommand::List => {
                cmd_template_list().await?;
                Ok(0)
            }
            TemplateCommand::Info { id } => {
                cmd_template_info(id).await?;
                Ok(0)
            }
            TemplateCommand::Rm { id, yes } => {
                cmd_template_rm(id, yes).await?;
                Ok(0)
            }
        },
        Commands::Run(args) => cmd_run(args).await,
        Commands::Shell(args) => cmd_shell(args).await,
        Commands::PoolStatus => {
            cmd_pool_status().await?;
            Ok(0)
        }
        Commands::Check => {
            cmd_check().await?;
            Ok(0)
        }
        Commands::Version => {
            cmd_version();
            Ok(0)
        }
    }
}

// ===================================================================
// Config helpers
// ===================================================================

fn build_config() -> RuntimeConfig {
    let mut config = RuntimeConfig::default();
    if let Ok(v) = std::env::var("COFFER_FIRECRACKER_PATH") {
        config.firecracker_path = v.into();
    }
    if let Ok(v) = std::env::var("COFFER_KERNEL_PATH") {
        config.kernel_path = v.into();
    }
    if let Ok(v) = std::env::var("COFFER_TEMPLATE_DIR") {
        config.template_dir = v.into();
    }
    if let Ok(v) = std::env::var("COFFER_SOCKET_DIR") {
        config.socket_dir = v.into();
    }
    if let Ok(v) = std::env::var("COFFER_AGENT_BIN") {
        config.agent_bin = v.into();
    }
    config
}

fn parse_key_val(s: &str) -> Result<(String, String)> {
    let pos = s
        .find('=')
        .ok_or_else(|| anyhow::anyhow!("invalid KEY=VALUE: no `=` found in `{}`", s))?;
    Ok((s[..pos].to_string(), s[pos + 1..].to_string()))
}

fn parse_file_mapping(s: &str) -> Result<(&str, &str)> {
    let pos = s
        .find(':')
        .ok_or_else(|| anyhow::anyhow!("invalid mapping format, expected `SRC:DST`: `{}`", s))?;
    Ok((&s[..pos], &s[pos + 1..]))
}

// ===================================================================
// Template commands
// ===================================================================

async fn cmd_template_build(
    image: String,
    name: String,
    kernel_args: Option<String>,
) -> Result<()> {
    let config = build_config();
    let templates = TemplateManager::new(
        config.template_dir,
        config.kernel_path,
        config.firecracker_path,
    )
    .with_agent_bin(config.agent_bin);

    println!("Building template '{}' from {} ...", name, image);
    let template = templates
        .build_from_image(&name, &image, kernel_args)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    println!("✓ Template built successfully");
    println!("  ID:      {}", template.id);
    println!("  Name:    {}", template.name);
    println!("  Rootfs:  {}", template.rootfs_path.display());
    Ok(())
}

async fn cmd_template_list() -> Result<()> {
    let config = build_config();
    let templates = TemplateManager::new(
        config.template_dir,
        config.kernel_path,
        config.firecracker_path,
    );

    let list = templates.list();
    if list.is_empty() {
        println!("No templates found.");
        println!("  Hint: coffer-cli template build --name <name> <oci-image>");
        return Ok(());
    }

    println!("{:<28} {:<16} {:>6} {:>10}", "ID", "NAME", "VCPUS", "MEM(MiB)");
    println!("{}", "-".repeat(64));
    for t in list {
        println!(
            "{:<28} {:<16} {:>6} {:>10}",
            t.id, t.name, t.vcpus, t.memory_mib
        );
    }
    Ok(())
}

async fn cmd_template_info(id: String) -> Result<()> {
    let config = build_config();
    let templates = TemplateManager::new(
        config.template_dir,
        config.kernel_path,
        config.firecracker_path,
    );
    let t = templates
        .get(&id)
        .map_err(|e| anyhow::anyhow!("{}", e))?;

    println!("Template: {}", t.id);
    println!("  Name:         {}", t.name);
    println!("  Kernel:       {}", t.kernel_path.display());
    println!("  Rootfs:       {}", t.rootfs_path.display());
    println!(
        "  Snapshot:     {} / {}",
        t.snapshot_state_path.display(),
        t.snapshot_mem_path.display()
    );
    println!("  Kernel args:  {}", t.kernel_args);
    println!("  vCPUs:        {}", t.vcpus);
    println!("  Memory:       {} MiB", t.memory_mib);
    if !t.metadata.is_empty() {
        println!("  Metadata:");
        for (k, v) in &t.metadata {
            println!("    {}: {}", k, v);
        }
    }
    Ok(())
}

async fn cmd_template_rm(id: String, yes: bool) -> Result<()> {
    let config = build_config();
    let path = config.template_dir.join(&id);
    if !path.exists() {
        anyhow::bail!("Template '{}' not found", id);
    }

    if !yes {
        print!("Remove template '{}' and all its files? [y/N] ", id);
        use std::io::Write;
        std::io::stdout().flush()?;
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        let trimmed = buf.trim().to_lowercase();
        if trimmed != "y" && trimmed != "yes" {
            println!("Cancelled.");
            return Ok(());
        }
    }

    tokio::fs::remove_dir_all(&path)
        .await
        .with_context(|| format!("Failed to remove template {}", id))?;
    println!("Template '{}' removed.", id);
    Ok(())
}

// ===================================================================
// Run command
// ===================================================================

async fn cmd_run(args: RunArgs) -> Result<i32> {
    let config = build_config();
    let runtime = Runtime::new(config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to initialize runtime: {}", e))?;

    let template_id = args.template;
    let cmd = if args.cmd.is_empty() {
        vec!["/bin/sh".to_string()]
    } else {
        args.cmd
    };

    // Acquire sandbox
    let acquire_start = std::time::Instant::now();
    if !args.json {
        eprint!("Acquiring sandbox (template: {}) ... ", template_id);
    }
    let handle = runtime
        .acquire(&template_id)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    let acquire_ms = acquire_start.elapsed().as_millis() as u64;
    let vm_id = handle.vm_id().to_string();
    if !args.json {
        eprintln!("{}", vm_id);
    }

    let sandbox = handle.sandbox();

    // Upload files
    for upload in &args.upload {
        let (local, remote) = parse_file_mapping(upload)?;
        let data = tokio::fs::read(local)
            .await
            .with_context(|| format!("Failed to read {}", local))?;
        if !args.json {
            eprintln!("Uploading {} -> {} ...", local, remote);
        }
        sandbox
            .upload_file(remote, data)
            .await
            .map_err(|e| anyhow::anyhow!("Upload failed: {}", e))?;
    }

    // Execute
    let exec_start = std::time::Instant::now();
    if !args.json {
        eprint!("Executing: {:?} ... ", cmd);
    }
    let env: HashMap<String, String> = args.env.into_iter().collect();
    let cmd_refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
    let output = sandbox
        .exec(&cmd_refs, &env, args.timeout)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    let exec_ms = exec_start.elapsed().as_millis() as u64;
    if !args.json {
        eprintln!(
            "done (exit_code={}, duration={}ms)",
            output.exit_code, output.duration_ms
        );
    }

    // Download files
    for download in &args.download {
        let (remote, local) = parse_file_mapping(download)?;
        if !args.json {
            eprintln!("Downloading {} -> {} ...", remote, local);
        }
        let data = sandbox
            .download_file(remote)
            .await
            .map_err(|e| anyhow::anyhow!("Download failed: {}", e))?;
        tokio::fs::write(local, &data)
            .await
            .with_context(|| format!("Failed to write {}", local))?;
    }

    // SandboxHandle drops here and returns VM to warm pool automatically.
    if !args.json {
        eprintln!("Sandbox {} released to warm pool.", vm_id);
    }

    if args.json {
        let result = serde_json::json!({
            "vm_id": vm_id,
            "template_id": template_id,
            "command": cmd,
            "exit_code": output.exit_code,
            "stdout": output.stdout,
            "stderr": output.stderr,
            "duration_ms": output.duration_ms,
            "acquire_ms": acquire_ms,
            "exec_ms": exec_ms,
        });
        println!("{}", serde_json::to_string_pretty(&result)?);
    } else {
        if !output.stdout.is_empty() {
            print!("{}", output.stdout);
        }
        if !output.stderr.is_empty() {
            eprint!("{}", output.stderr);
        }
    }

    Ok(output.exit_code)
}

// ===================================================================
// Shell command
// ===================================================================

async fn cmd_shell(args: ShellArgs) -> Result<i32> {
    let config = build_config();
    let runtime = Runtime::new(config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to initialize runtime: {}", e))?;

    let template_id = args.template;
    let shell = args.shell;

    eprintln!("Acquiring sandbox (template: {}) ... ", template_id);
    let handle = runtime
        .acquire(&template_id)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    let vm_id = handle.vm_id().to_string();
    eprintln!("{}", vm_id);

    let sandbox = handle.sandbox();

    let env: HashMap<String, String> = args.env.into_iter().collect();

    eprintln!("Starting interactive shell ({}). Press Ctrl+D or type 'exit' to quit.", shell);

    let shell_start = std::time::Instant::now();
    let exit_code = sandbox
        .exec_interactive(&[&shell], &env, args.working_dir)
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))?;
    let shell_duration = shell_start.elapsed();
    eprintln!("Shell exited with code {} after {:?}", exit_code, shell_duration);

    eprintln!("\nSandbox {} released to warm pool.", vm_id);
    Ok(exit_code)
}

// ===================================================================
// Pool status
// ===================================================================

async fn cmd_pool_status() -> Result<()> {
    let config = build_config();
    let runtime = Runtime::new(config)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to initialize runtime: {}", e))?;

    let pool = runtime.pool();
    let available = pool.available_counts();
    let in_use = pool.in_use_count();

    println!("Warm Pool Status");
    println!("----------------");
    println!("In-use sandboxes: {}", in_use);
    println!("Available sandboxes by template:");
    if available.is_empty() {
        println!("  (none)");
    } else {
        println!("  {:<28} COUNT", "TEMPLATE");
        println!("  {}", "-".repeat(40));
        for (template_id, count) in &available {
            println!("  {:<28} {}", template_id, count);
        }
    }
    Ok(())
}

// ===================================================================
// Check command
// ===================================================================

async fn cmd_check() -> Result<()> {
    let config = build_config();
    let mut ok = true;

    fn check(label: &str, result: Result<()>) -> bool {
        match result {
            Ok(()) => {
                println!("  [OK]   {}", label);
                true
            }
            Err(e) => {
                println!("  [FAIL] {} — {}", label, e);
                false
            }
        }
    }

    println!("System readiness check");
    println!("----------------------");

    // Firecracker
    ok &= check(
        &format!("Firecracker binary ({})", config.firecracker_path.display()),
        check_binary(&config.firecracker_path),
    );

    // Jailer (optional)
    if let Some(ref jailer) = config.jailer_path {
        ok &= check(
            &format!("Jailer binary ({}) [optional]", jailer.display()),
            check_binary(jailer),
        );
    }

    // Kernel
    ok &= check(
        &format!("Kernel image ({})", config.kernel_path.display()),
        check_file(&config.kernel_path),
    );

    // Agent
    ok &= check(
        &format!("Agent binary ({})", config.agent_bin.display()),
        check_file(&config.agent_bin),
    );

    // KVM (must be readable and writable — Firecracker opens it O_RDWR)
    ok &= check(
        "/dev/kvm read+write",
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .map(|_| ())
            .map_err(|e| {
                anyhow::anyhow!(
                    "{}. To fix:\n    sudo usermod -aG kvm $(whoami) && newgrp kvm\n  or:\n    sudo chmod 666 /dev/kvm",
                    e
                )
            }),
    );

    // Template dir
    ok &= check(
        &format!("Template directory ({})", config.template_dir.display()),
        check_dir(&config.template_dir),
    );

    // External tools for template build
    ok &= check("mkfs.erofs", check_cmd("mkfs.erofs", &["--help"]));
    ok &= check("skopeo", check_cmd("skopeo", &["--version"]));
    ok &= check("umoci", check_cmd("umoci", &["--version"]));

    // Network tools
    ok &= check("ip", check_cmd("ip", &["-V"]));
    ok &= check("iptables", check_cmd("iptables", &["--version"]));

    // Network privileges (CAP_NET_ADMIN)
    let has_net_admin = check_cap_net_admin();
    if !has_net_admin {
        println!("  [WARN] CAP_NET_ADMIN not available — network setup will require root");
        println!("         To run without sudo:");
        println!("           sudo setcap cap_net_admin+eip {}", config.firecracker_path.display());
        println!("         Or pre-create the bridge as root:");
        println!("           sudo ip link add {} type bridge && sudo ip link set {} up",
            config.network.bridge_name, config.network.bridge_name);
    } else {
        println!("  [OK]   CAP_NET_ADMIN available");
    }

    println!();
    if ok {
        println!("✓ All checks passed. Coffer is ready to use.");
    } else {
        println!("✗ Some checks failed. Please fix the issues above before running Coffer.");
        std::process::exit(1);
    }
    Ok(())
}

fn check_binary(path: &std::path::Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("binary not found");
    }
    let meta = std::fs::metadata(path)?;
    let perms = meta.permissions();
    if perms.readonly() {
        anyhow::bail!("binary is not executable");
    }
    // On Unix, check executable bit
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if perms.mode() & 0o111 == 0 {
            anyhow::bail!("binary lacks executable permission");
        }
    }
    Ok(())
}

fn check_file(path: &std::path::Path) -> Result<()> {
    if !path.exists() {
        anyhow::bail!("file not found");
    }
    Ok(())
}

fn check_dir(path: &std::path::Path) -> Result<()> {
    if !path.exists() {
        std::fs::create_dir_all(path)?;
    }
    if !path.is_dir() {
        anyhow::bail!("not a directory");
    }
    Ok(())
}

fn check_cmd(cmd: &str, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new(cmd)
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match status {
        Ok(s) if s.success() => Ok(()),
        Ok(_) => anyhow::bail!("command exited with non-zero status"),
        Err(e) => Err(e.into()),
    }
}

/// Check whether the current process has CAP_NET_ADMIN.
/// Reads /proc/self/status and looks at the effective capability set.
fn check_cap_net_admin() -> bool {
    let contents = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let mut uid = None;
    let mut cap_eff = None;
    for line in contents.lines() {
        if let Some(val) = line.strip_prefix("Uid:") {
            uid = val.trim().split_whitespace().next().and_then(|s| s.parse::<u32>().ok());
        }
        if let Some(val) = line.strip_prefix("CapEff:") {
            cap_eff = u64::from_str_radix(val.trim(), 16).ok();
        }
    }
    if uid == Some(0) {
        return true;
    }
    if let Some(caps) = cap_eff {
        // CAP_NET_ADMIN = 12
        return (caps & (1u64 << 12)) != 0;
    }
    false
}

// ===================================================================
// Version command
// ===================================================================

fn cmd_version() {
    let pkg_version = env!("CARGO_PKG_VERSION");
    let git_commit = env!("GIT_COMMIT");
    let git_log = env!("GIT_LOG").replace("\\n", "\n");

    println!("coffer-cli {}", pkg_version);
    println!("git commit: {}", git_commit);
    println!();
    println!("Recent commits:");
    for line in git_log.lines() {
        println!("  {}", line);
    }
}

// ===================================================================
// Tests
// ===================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_key_val() {
        assert_eq!(parse_key_val("FOO=bar").unwrap(), ("FOO".into(), "bar".into()));
        assert_eq!(
            parse_key_val("KEY=val=with=equals").unwrap(),
            ("KEY".into(), "val=with=equals".into())
        );
    }

    #[test]
    fn test_parse_key_val_invalid() {
        assert!(parse_key_val("FOO").is_err());
    }

    #[test]
    fn test_parse_file_mapping() {
        assert_eq!(
            parse_file_mapping("./local.txt:/tmp/remote.txt").unwrap(),
            ("./local.txt", "/tmp/remote.txt")
        );
    }

    #[test]
    fn test_parse_file_mapping_invalid() {
        assert!(parse_file_mapping("nocolon").is_err());
    }
}
