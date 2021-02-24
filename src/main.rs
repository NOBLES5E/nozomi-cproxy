use structopt::StructOpt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[derive(StructOpt, Debug)]
struct Cli {
    #[structopt(long, default_value = "1081")]
    port: u32,
    #[structopt(long)]
    use_tproxy: bool,
    #[structopt(long)]
    pid: Option<u32>,
    #[structopt(subcommand)]
    command: Option<ChildCommand>,
}

#[derive(StructOpt, Debug)]
enum ChildCommand {
    #[structopt(external_subcommand)]
    Command(Vec<String>)
}

struct RedirectGuard {
    class_id: u32,
    port: u32,
    pid: u32,
    output_chain_name: String,
    cgroup_path: String,
}

impl RedirectGuard {
    fn new(class_id: u32, port: u32, pid: u32, output_chain_name: &str, cgroup_path: &str) -> anyhow::Result<Self> {
        (cmd_lib::run_cmd! {
        sudo mkdir -p /sys/fs/cgroup/net_cls/${cgroup_path};
        echo ${class_id} | sudo tee /sys/fs/cgroup/net_cls/${cgroup_path}/net_cls.classid > /dev/null;
        echo ${pid} | sudo tee /sys/fs/cgroup/net_cls/${cgroup_path}/cgroup.procs > /dev/null;

        sudo iptables -t nat -N ${output_chain_name};
        sudo iptables -t nat -A OUTPUT -j ${output_chain_name};
        sudo iptables -t nat -A ${output_chain_name} -p tcp -m cgroup --cgroup ${class_id} -j REDIRECT --to-ports ${port};
        sudo iptables -t nat -A ${output_chain_name} -p udp -m cgroup --cgroup ${class_id} --dport 53 -j REDIRECT --to-ports ${port};
        })?;

        Ok(Self {
            class_id,
            port,
            pid,
            output_chain_name: output_chain_name.to_owned(),
            cgroup_path: cgroup_path.to_owned(),
        })
    }
}

impl Drop for RedirectGuard {
    fn drop(&mut self) {
        let output_chain_name = &self.output_chain_name;
        let pid = self.pid;
        let cgroup_path = &self.cgroup_path;

        (cmd_lib::run_cmd! {
        sudo iptables -t nat -D OUTPUT -j ${output_chain_name};
        sudo iptables -t nat -F ${output_chain_name};
        sudo iptables -t nat -X ${output_chain_name};

        echo ${pid} | sudo tee /sys/fs/cgroup/net_cls/cgroup.procs > /dev/null;
        sudo rmdir /sys/fs/cgroup/net_cls/${cgroup_path};
        }).expect("drop iptables and cgroup failed");
    }
}

fn proxy_new_command(args: &Cli) -> anyhow::Result<()> {
    let pid = std::process::id();
    let ChildCommand::Command(child_command) = &args.command.as_ref().expect("must have command specified if --pid not provided");
    tracing::info!("subcommand {:?}", child_command);

    let cgroup_path = format!("nozomi_tproxy_{}", pid);
    let class_id = args.port;
    let port = args.port;
    let output_chain_name = format!("nozomi_tproxy_out_{}", pid);

    let _guard = RedirectGuard::new(class_id, port, pid, output_chain_name.as_str(), cgroup_path.as_str());

    let mut child = std::process::Command::new(&child_command[0]).args(&child_command[1..]).spawn()?;

    ctrlc::set_handler(move || {
        println!("received ctrl-c, terminating...");
    })?;

    child.wait()?;


    Ok(())
}

fn proxy_existing_pid(pid: u32, args: &Cli) -> anyhow::Result<()> {
    let cgroup_path = format!("nozomi_tproxy_{}", pid);
    let class_id = args.port;
    let port = args.port;
    let output_chain_name = format!("nozomi_tproxy_out_{}", pid);
    let _guard = RedirectGuard::new(class_id, port, pid, output_chain_name.as_str(), cgroup_path.as_str());

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        println!("received ctrl-c, terminating...");
        r.store(false, Ordering::SeqCst);
    })?;

    while running.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }

    Ok(())
}

struct TProxyGuard {
    class_id: u32,
    port: u32,
    pid: u32,
    mark: u32,
    output_chain_name: String,
    prerouting_chain_name: String,
    cgroup_path: String,
}

impl TProxyGuard {
    fn new(class_id: u32, port: u32, pid: u32, mark: u32, output_chain_name: &str, prerouting_chain_name: &str, cgroup_path: &str) -> anyhow::Result<Self> {
        (cmd_lib::run_cmd! {
        sudo ip rule add fwmark ${mark} table ${mark};
        sudo ip route add local 0.0.0.0/0 dev lo table ${mark};

        sudo mkdir -p /sys/fs/cgroup/net_cls/${cgroup_path};
        echo ${class_id} | sudo tee /sys/fs/cgroup/net_cls/${cgroup_path}/net_cls.classid > /dev/null;
        echo ${pid} | sudo tee /sys/fs/cgroup/net_cls/${cgroup_path}/cgroup.procs > /dev/null;

        sudo iptables -t mangle -N ${prerouting_chain_name};
        sudo iptables -t mangle -A PREROUTING -j ${prerouting_chain_name};
        sudo iptables -t mangle -A ${prerouting_chain_name} -p udp -m mark --mark ${mark} -j TPROXY --on-ip 127.0.0.1 --on-port ${port};
        sudo iptables -t mangle -A ${prerouting_chain_name} -p tcp -m mark --mark ${mark} -j TPROXY --on-ip 127.0.0.1 --on-port ${port};

        sudo iptables -t mangle -N ${output_chain_name};
        sudo iptables -t mangle -A OUTPUT -j ${output_chain_name};
        sudo iptables -t mangle -A ${output_chain_name} -p tcp -m cgroup --cgroup ${class_id} -j MARK --set-mark ${mark};
        sudo iptables -t mangle -A ${output_chain_name} -p udp -m cgroup --cgroup ${class_id} -j MARK --set-mark ${mark};
        })?;

        Ok(Self {
            class_id,
            port,
            pid,
            mark,
            output_chain_name: output_chain_name.to_owned(),
            prerouting_chain_name: prerouting_chain_name.to_owned(),
            cgroup_path: cgroup_path.to_owned(),
        })
    }
}

impl Drop for TProxyGuard {
    fn drop(&mut self) {
        let output_chain_name = &self.output_chain_name;
        let prerouting_chain_name = &self.prerouting_chain_name;
        let pid = self.pid;
        let mark = self.mark;
        let cgroup_path = &self.cgroup_path;

        (cmd_lib::run_cmd! {
        sudo ip rule delete fwmark ${mark} table ${mark};
        sudo ip route delete local 0.0.0.0/0 dev lo table ${mark};

        sudo iptables -t mangle -D PREROUTING -j ${prerouting_chain_name};
        sudo iptables -t mangle -F ${prerouting_chain_name};
        sudo iptables -t mangle -X ${prerouting_chain_name};

        sudo iptables -t mangle -D OUTPUT -j ${output_chain_name};
        sudo iptables -t mangle -F ${output_chain_name};
        sudo iptables -t mangle -X ${output_chain_name};

        echo ${pid} | sudo tee /sys/fs/cgroup/net_cls/cgroup.procs > /dev/null;
        sudo rmdir /sys/fs/cgroup/net_cls/${cgroup_path};
    }).expect("drop iptables and cgroup failed");
    }
}

fn proxy_new_command_tproxy(args: &Cli) -> anyhow::Result<()> {
    let pid = std::process::id();
    let ChildCommand::Command(child_command) = &args.command.as_ref().expect("must have command specified if --pid not provided");
    tracing::info!("subcommand {:?}", child_command);

    let cgroup_path = format!("nozomi_tproxy_{}", pid);
    let prerouting_chain_name = format!("nozomi_tproxy_pre_{}", pid);
    let output_chain_name = format!("nozomi_tproxy_out_{}", pid);
    let class_id = args.port;
    let port = args.port;
    let mark = pid;

    let _guard = TProxyGuard::new(class_id, port, pid, mark, output_chain_name.as_str(), prerouting_chain_name.as_str(), cgroup_path.as_str());

    let mut child = std::process::Command::new(&child_command[0]).args(&child_command[1..]).spawn()?;
    ctrlc::set_handler(move || {
        println!("received ctrl-c, terminating...");
    })?;
    child.wait()?;
    Ok(())
}

fn proxy_existing_pid_tproxy(pid: u32, args: &Cli) -> anyhow::Result<()> {
    let cgroup_path = format!("nozomi_tproxy_{}", pid);
    let prerouting_chain_name = format!("nozomi_tproxy_pre_{}", pid);
    let output_chain_name = format!("nozomi_tproxy_out_{}", pid);
    let class_id = args.port;
    let port = args.port;
    let mark = pid;

    let _guard = TProxyGuard::new(class_id, port, pid, mark, output_chain_name.as_str(), prerouting_chain_name.as_str(), cgroup_path.as_str());

    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    ctrlc::set_handler(move || {
        println!("received ctrl-c, terminating...");
        r.store(false, Ordering::SeqCst);
    })?;

    while running.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(100));
    }

    Ok(())
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("LOG_LEVEL"))
        .init();
    let args: Cli = Cli::from_args();

    match args.pid {
        None => {
            match args.use_tproxy {
                true => {
                    proxy_new_command_tproxy(&args)?;
                }
                false => {
                    proxy_new_command(&args)?;
                }
            }
        }
        Some(existing_pid) => {
            match args.use_tproxy {
                true => {
                    proxy_existing_pid_tproxy(existing_pid, &args)?;
                }
                false => {
                    proxy_existing_pid(existing_pid, &args)?;
                }
            }
        }
    }

    Ok(())
}
