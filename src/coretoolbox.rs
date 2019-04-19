use structopt::StructOpt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::os::unix::process::CommandExt;
use std::io::prelude::*;
use directories;
use failure::{Fallible, bail};
use lazy_static::lazy_static;
use serde_json;
use serde::{Serialize, Deserialize};

lazy_static! {
    static ref APPDIRS : directories::ProjectDirs = directories::ProjectDirs::from("com", "coreos", "toolbox").expect("creating appdirs");
}

static MAX_UID_COUNT : u32 = 65536;

static PRESERVED_ENV : &[&str] = &["COLORTERM", 
        "DBUS_SESSION_BUS_ADDRESS",
        "DESKTOP_SESSION",
        "DISPLAY",
        "LANG",
        "SHELL",
        "SSH_AUTH_SOCK",
        "TERM",
        "VTE_VERSION",
        "XDG_CURRENT_DESKTOP",
        "XDG_DATA_DIRS",
        "XDG_MENU_PREFIX",
        "XDG_RUNTIME_DIR",
        "XDG_SEAT",
        "XDG_SESSION_DESKTOP",
        "XDG_SESSION_ID",
        "XDG_SESSION_TYPE",
        "XDG_VTNR",
];

#[derive(Debug, StructOpt)]
#[structopt(name = "coretoolbox", about = "Toolbox")]
#[structopt(rename_all = "kebab-case")]
/// Main options struct
struct Opt {
    #[structopt(short = "I", long = "image", default_value = "registry.fedoraproject.org/f30/fedora-toolbox:30")]
    /// Use a versioned installer binary
    image: String,

    #[structopt(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Debug, StructOpt)]
#[structopt(rename_all = "kebab-case")]
enum Cmd {
    Entrypoint,
}

fn cmd_podman() -> Command {
    if let Some(podman) = std::env::var_os("podman") {
        Command::new(podman)
    } else {
        Command::new("podman")
    }
}

/// Returns true if the host is OSTree based
fn is_ostree_based_host() -> bool {
    std::path::Path::new("/run/ostree-booted").exists()
}

enum InspectType {
    Container,
    Image,
}

fn podman_has(t: InspectType, name: &str) -> Fallible<bool> {
    let typearg = match t {
        InspectType::Container => "container",
        InspectType::Image => "image",
    };
    Ok(cmd_podman().args(&["inspect", "--type", typearg, name])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?.success())
}

/// Pull a container image if not present
fn ensure_image(name: &str) -> Fallible<()> {
    if !podman_has(InspectType::Image, name)? {
        if !cmd_podman().args(&["pull", name]).status()?.success() {
            bail!("Failed to pull image");
        }
    }
    Ok(())
}

fn getenv_required_utf8(n: &str) -> Fallible<String> {
    if let Some(v) = std::env::var_os(n) {
        Ok(v.to_str().ok_or_else(|| failure::format_err!("{} is invalid UTF-8", n))?.to_string())
    } else {
        bail!("{} is unset", n)
    }
}

#[derive(Serialize, Deserialize, Debug)]
struct EntrypointState {
    username: String,
    uid: u32,
    ostree_based_host: bool,
}

fn run(opts: Opt) -> Fallible<()> {
    ensure_image(&opts.image)?;

    // exec ourself as the entrypoint.  In the future this
    // would be better with podman fd passing.
    let self_bin = std::fs::read_link("/proc/self/exe")?;
    let self_bin = self_bin.as_path().to_str().ok_or_else(|| failure::err_msg("non-UTF8 self"))?;

    // Serialize our
    let pid : i32 = nix::unistd::getpid().as_raw().into();
    let runtime_dir = getenv_required_utf8("XDG_RUNTIME_DIR")?;
    let r : u32 = rand::random();
    let statefile = format!("toolbox-data-{}-{:x}", pid, r);

    let mut podman = cmd_podman();
    podman.args(&["run", "--rm", "-ti", "--hostname", "toolbox",
                  "--name", "coreos-toolbox", "--network", "host",
                  "--privileged", "--security-opt", "label-disable"]);
    podman.arg(format!("--volume={}:/toolbox.entrypoint:rslave", self_bin));
    let real_uid : u32 = nix::unistd::getuid().into();
    let uid_plus_one = real_uid + 1;             
    let max_minus_uid = MAX_UID_COUNT - real_uid;     
    podman.args(&[format!("--uidmap={}:0:1", real_uid),
                  format!("--uidmap=0:1:{}", real_uid),
                  format!("--uidmap={}:{}:{}", uid_plus_one, uid_plus_one, max_minus_uid)]);
    // TODO: Detect what devices are accessible
    for p in &["/dev/bus", "/dev/dri", "/dev/fuse"] {
        if Path::new(p).exists() {
            podman.arg(format!("--volume={}:{}:rslave", p, p));
        }
    }
    for p in &["/usr", "/var", "/etc", "/run"] {
        podman.arg(format!("--volume={}:/host{}:rslave", p, p));
    }    
    if is_ostree_based_host() {
        podman.arg(format!("--volume=/sysroot:/host/sysroot:rslave"));
    } else {
        for p in &["/media", "/mnt", "/home", "/srv"] {
            podman.arg(format!("--volume={}:/host{}:rslave", p, p));
        }           
    }
    for n in PRESERVED_ENV.iter() {
        let v = match std::env::var_os(n) {
            Some(v) => v,
            None => continue, 
        };
        let v = v.to_str().ok_or_else(|| failure::format_err!("{} contains invalid UTF-8", n))?;
        podman.arg(format!("--env={}={}", n, v));
    }
    podman.arg(format!("--env=TOOLBOX_STATEFILE={}", statefile));

    {
        let state = EntrypointState {
            username: getenv_required_utf8("USER")?,
            uid: real_uid,
            ostree_based_host: is_ostree_based_host(),
        };
        let w = std::fs::File::create(format!("{}/{}", runtime_dir, statefile))?;
        let mut w = std::io::BufWriter::new(w);
        serde_json::to_writer(&mut w, &state)?;
        w.flush()?;
    }

    podman.arg("--entrypoint=/toolbox.entrypoint");
    podman.arg(opts.image);
    eprintln!("running {:?}", podman);
    return Err(podman.exec().into())
}

mod entrypoint {
    use failure::{Fallible, bail};
    use std::process::Command;
    use std::os::unix::process::CommandExt;

    fn adduser(name: &str, uid: u32) -> Fallible<()> {
        let uidstr = format!("{}", uid);
        if !Command::new("useradd")
            .args(&["--no-create-home", "--uid", &uidstr,
                    "--groups", "wheel", name])
            .status()?.success() {
                bail!("Failed to useradd");
        }
        Ok(())
    }

    pub(crate) fn entrypoint() -> Fallible<()> {
        let statefile = super::getenv_required_utf8("TOOLBOX_STATEFILE")?;
        let runtime_dir = super::getenv_required_utf8("XDG_RUNTIME_DIR")?;
        let state : super::EntrypointState = {
            let f = std::fs::File::open(format!("/host/{}/{}", runtime_dir, statefile))?;
            serde_json::from_reader(std::io::BufReader::new(f))?
        };

        adduser(&state.username, state.uid)?;

        let shell = std::env::var_os("SHELL").unwrap_or("sh".into())
            .into_string().map_err(|_| failure::err_msg("Invalid SHELL"))?;
        Command::new(shell).exec();
        Err(Command::new("sh").exec().into())
    }
}

/// Primary entrypoint
fn main() -> Fallible<()> {
    let argv0 = std::env::args().next().expect("argv0");
    if argv0.ends_with(".entrypoint") {
        return entrypoint::entrypoint();
    }
    let opts = Opt::from_args();
    if let Some(cmd) = opts.cmd.as_ref() {
        match cmd {
            Cmd::Entrypoint => {
                return entrypoint::entrypoint();
            }
        }
    } else {
        run(opts)?;
    }
    Ok(())
}
