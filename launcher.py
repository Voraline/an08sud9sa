#!/usr/bin/env python3
import argparse
import getpass
import json
import os
import shlex
import shutil
import subprocess
import sys
from pathlib import Path

SCRIPT_DIR = Path(__file__).resolve().parent
CONFIG_PATH = SCRIPT_DIR / "config.json"

DEFAULT_CONFIG = {
    "guild_id": "1238452317838377035",
    "channel_id": "1519697303080796303",
    "audio": (
        "D:\\Stuff\\OpusAudio\\DJ RECAP 2025 VIRAL TIK-TOK TERBARU "
        "FULL PARTY JEDAG JEDUG \u2022 BONGOBARBAR_Horeg.ogg"
    ),
    "selfbot": True,
    "ffmpeg": "ffmpeg",
    "tokens": [],
}


def LoadConfig(PathValue):
    Config = dict(DEFAULT_CONFIG)
    if PathValue.is_file():
        with PathValue.open("r", encoding="utf-8") as Handle:
            Config.update(json.load(Handle))
    return Config


def ResolveFfmpeg(Config, Override):
    Candidates = [Override, os.environ.get("FFMPEG_PATH"), Config.get("ffmpeg"), "ffmpeg"]
    for Candidate in Candidates:
        if not Candidate:
            continue
        PathValue = Path(Candidate).expanduser()
        if PathValue.is_file():
            return str(PathValue)
        Found = shutil.which(str(Candidate))
        if Found:
            return Found
    return None


def ResolveVoicePlay(Override):
    Candidates = []
    if Override:
        Candidates.append(Path(Override).expanduser())
    Environment = os.environ.get("VOICEPLAY_PATH")
    if Environment:
        Candidates.append(Path(Environment).expanduser())
    Candidates.extend(
        [
            SCRIPT_DIR / "VoicePlay.exe",
            SCRIPT_DIR / "VoicePlay",
            SCRIPT_DIR / "build" / "Release" / "VoicePlay.exe",
            SCRIPT_DIR / "build" / "Release" / "VoicePlay",
            SCRIPT_DIR / "build" / "VoicePlay",
        ]
    )
    for Candidate in Candidates:
        if Candidate.is_file():
            if os.name != "nt":
                Candidate.chmod(Candidate.stat().st_mode | 0o111)
            return str(Candidate)
    return None


def ResolveTokens(Arguments, Config):
    Tokens = Arguments.token
    if not Tokens:
        Environment = os.environ.get("VOICE_TOKENS")
        if Environment:
            Tokens = [Item.strip() for Item in Environment.split(",") if Item.strip()]
    if not Tokens:
        Tokens = Config.get("tokens") or []
    Tokens = [str(Token).strip() for Token in Tokens if str(Token).strip()]
    if not Tokens:
        Entered = getpass.getpass("Discord token: ").strip()
        if Entered:
            Tokens = [Entered]
    if not Tokens:
        raise RuntimeError(
            "No tokens configured. Add them to config.json, pass --token, "
            "or set VOICE_TOKENS."
        )
    return Tokens


def IsRoot():
    return hasattr(os, "geteuid") and os.geteuid() == 0


def WithPrivilege(Command):
    if IsRoot() or not shutil.which("sudo"):
        return Command
    return ["sudo"] + Command


def DetectInterface():
    try:
        Result = subprocess.run(
            ["ip", "-o", "route", "get", "1.1.1.1"],
            capture_output=True,
            text=True,
            timeout=5,
        )
    except (OSError, subprocess.SubprocessError):
        return None
    Words = Result.stdout.split()
    if "dev" in Words:
        Index = Words.index("dev") + 1
        if Index < len(Words):
            return Words[Index]
    return None


def ParseQdisc(Text, Slack):
    for Line in Text.splitlines():
        Words = Line.split()
        if len(Words) >= 2 and Words[0] == "qdisc" and Words[1] == "fq":
            SlackMatches = False
            if "timer_slack" in Words:
                Index = Words.index("timer_slack") + 1
                SlackMatches = Index < len(Words) and Words[Index] == Slack
            return True, SlackMatches
    return False, False


def ShowQdisc(Interface):
    try:
        Result = subprocess.run(
            ["tc", "qdisc", "show", "dev", Interface],
            capture_output=True,
            text=True,
            timeout=10,
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    return Result.stdout


def SetupFq(InterfaceOverride, Slack):
    if not sys.platform.startswith("linux"):
        return False, "only available on Linux"
    if not shutil.which("tc"):
        return False, "`tc` not found (install iproute2)"

    Interface = InterfaceOverride or DetectInterface()
    if not Interface:
        return False, "could not detect the network interface (use --iface)"

    IsFq, SlackMatches = ParseQdisc(ShowQdisc(Interface), Slack)
    if IsFq and SlackMatches:
        return True, f"already active on {Interface} (fq, timer_slack {Slack})"

    Command = WithPrivilege(
        ["tc", "qdisc", "replace", "dev", Interface, "root", "fq", "timer_slack", Slack]
    )
    try:
        Result = subprocess.run(Command, capture_output=True, text=True, timeout=60)
    except (OSError, subprocess.SubprocessError) as Error:
        return False, f"could not run tc on {Interface}: {Error}"
    if Result.returncode != 0:
        Reason = (Result.stderr or Result.stdout).strip() or f"exit code {Result.returncode}"
        return False, f"could not enable on {Interface}: {Reason}"

    IsFq, SlackMatches = ParseQdisc(ShowQdisc(Interface), Slack)
    if not IsFq:
        return False, f"tc succeeded but fq is not active on {Interface}"
    if not SlackMatches:
        return True, f"active on {Interface}, but timer_slack is not {Slack} (old tc?)"
    return True, f"enabled on {Interface} (fq, timer_slack {Slack})"


KERNEL_TUNES = [
    ("kernel/sched_autogroup_enabled", "0"),
    ("kernel/timer_migration", "0"),
    ("kernel/sched_rt_runtime_us", "-1"),
]

VM_TUNES = [
    ("vm/swappiness", "0"),
    ("vm/vfs_cache_pressure", "50"),
    ("vm/dirty_background_ratio", "2"),
    ("vm/dirty_ratio", "5"),
    ("vm/overcommit_memory", "1"),
    ("vm/max_map_count", "262144"),
]

FS_TUNES = [
    ("fs/file-max", "2097152"),
    ("fs/inotify/max_user_watches", "524288"),
]

TCP_TUNES = [
    ("net/ipv4/tcp_fastopen", "1"),
    ("net/ipv4/tcp_fin_timeout", "15"),
    ("net/ipv4/tcp_rmem", "4096 16777216 67108864"),
    ("net/ipv4/tcp_wmem", "4096 16777216 67108864"),
    ("net/ipv4/tcp_mem", "2097152 4194304 8388608"),
    ("net/core/somaxconn", "65535"),
    ("net/ipv4/tcp_max_syn_backlog", "8192"),
    ("net/ipv4/tcp_keepalive_time", "60"),
    ("net/ipv4/tcp_keepalive_intvl", "10"),
    ("net/ipv4/tcp_keepalive_probes", "6"),
]

WRITEBACK_TUNES = [
    ("vm/dirty_expire_centisecs", "1500"),
    ("vm/dirty_writeback_centisecs", "250"),
    ("fs/inotify/max_user_instances", "512"),
]

NETWORK_TUNES = [
    ("net/core/wmem_max", "4194304"),
    ("net/core/rmem_max", "4194304"),
    ("net/core/rmem_default", "4194304"),
    ("net/core/wmem_default", "4194304"),
    ("net/core/optmem_max", "8388608"),
    ("net/core/netdev_max_backlog", "5000"),
    ("net/core/busy_read", "50"),
    ("net/core/busy_poll", "50"),
    ("net/ipv4/udp_rmem_min", "65536"),
    ("net/ipv4/udp_wmem_min", "65536"),
    ("net/ipv4/tcp_low_latency", "1"),
    ("net/ipv4/tcp_slow_start_after_idle", "0"),
    ("net/unix/max_dgram_qlen", "1024"),
]


def ApplySysctlTunes(Tunes, Label):
    if not sys.platform.startswith("linux"):
        return [(Label, False, "only available on Linux")]
    Results = []
    for Key, Value in Tunes:
        Name = Key.replace("/", ".")
        PathValue = Path("/proc/sys") / Key
        if not PathValue.is_file():
            Results.append((Name, False, "not available on this kernel"))
            continue
        try:
            Current = PathValue.read_text().strip()
        except OSError as Error:
            Results.append((Name, False, f"cannot read: {Error}"))
            continue
        if Current == Value:
            Results.append((Name, True, f"already {Value}"))
            continue
        try:
            Result = subprocess.run(
                WithPrivilege(["sysctl", "-w", f"{Name}={Value}"]),
                capture_output=True,
                text=True,
                timeout=30,
            )
        except (OSError, subprocess.SubprocessError) as Error:
            Results.append((Name, False, f"could not run sysctl: {Error}"))
            continue
        if Result.returncode != 0:
            Reason = (Result.stderr or Result.stdout).strip() or f"exit code {Result.returncode}"
            Results.append((Name, False, Reason))
            continue
        try:
            Now = PathValue.read_text().strip()
        except OSError:
            Now = Value
        if Now == Value:
            Results.append((Name, True, f"set to {Value} (was {Current})"))
        else:
            Results.append((Name, False, f"sysctl ran but value is still {Now}"))
    return Results


def TuneKernel():
    return ApplySysctlTunes(KERNEL_TUNES, "kernel tuning")


def TuneNetwork():
    return ApplySysctlTunes(NETWORK_TUNES, "network tuning")


def TuneProcess():
    """Apply ionice + renice to the current process for lower latency."""
    if not sys.platform.startswith("linux"):
        return []
    Results = []
    Pid = str(os.getpid())

    # ionice: realtime class (-c 1), highest priority (-n 0)
    if shutil.which("ionice"):
        try:
            Result = subprocess.run(
                WithPrivilege(["ionice", "-c", "1", "-n", "0", "-p", Pid]),
                capture_output=True, text=True, timeout=10,
            )
            if Result.returncode == 0:
                Results.append(("ionice", True, "realtime class 1, priority 0"))
            else:
                Reason = (Result.stderr or Result.stdout).strip()
                Results.append(("ionice", False, Reason or "failed"))
        except (OSError, subprocess.SubprocessError) as Error:
            Results.append(("ionice", False, str(Error)))
    else:
        Results.append(("ionice", False, "not found"))

    # renice: highest CPU priority
    if shutil.which("renice"):
        try:
            Result = subprocess.run(
                WithPrivilege(["renice", "-n", "-20", "-p", Pid]),
                capture_output=True, text=True, timeout=10,
            )
            if Result.returncode == 0:
                Results.append(("renice", True, "priority -20"))
            else:
                Reason = (Result.stderr or Result.stdout).strip()
                Results.append(("renice", False, Reason or "failed"))
        except (OSError, subprocess.SubprocessError) as Error:
            Results.append(("renice", False, str(Error)))
    else:
        Results.append(("renice", False, "not found"))

    return Results


def TuneIRQ(InterfaceOverride):
    """Stop irqbalance and pin NIC IRQs to CPU 0 to reduce interrupt jitter."""
    if not sys.platform.startswith("linux"):
        return [("irq", False, "only available on Linux")]
    Results = []

    # Stop irqbalance so it doesn't fight our manual pinning
    if shutil.which("systemctl"):
        try:
            Result = subprocess.run(
                WithPrivilege(["systemctl", "stop", "irqbalance"]),
                capture_output=True, text=True, timeout=10,
            )
            Results.append((
                "irqbalance",
                Result.returncode == 0,
                "stopped" if Result.returncode == 0 else (Result.stderr or "failed").strip(),
            ))
        except (OSError, subprocess.SubprocessError) as Error:
            Results.append(("irqbalance", False, str(Error)))

    # Find the NIC and pin its IRQs to CPU 0
    Interface = InterfaceOverride or DetectInterface()
    if not Interface:
        Results.append(("nic-irq", False, "could not detect interface"))
        return Results

    IrqDir = Path(f"/sys/class/net/{Interface}/device/msi_irqs")
    if not IrqDir.is_dir():
        Results.append(("nic-irq", False, f"no msi_irqs for {Interface}"))
        return Results

    Pinned = 0
    for IrqPath in IrqDir.iterdir():
        AffinityFile = Path(f"/proc/irq/{IrqPath.name}/smp_affinity_list")
        if not AffinityFile.exists():
            continue
        try:
            Result = subprocess.run(
                WithPrivilege(["tee", str(AffinityFile)]),
                input="0", capture_output=True, text=True, timeout=5,
            )
            if Result.returncode == 0:
                Pinned += 1
        except (OSError, subprocess.SubprocessError):
            pass

    if Pinned > 0:
        Results.append(("nic-irq", True, f"pinned {Pinned} IRQ(s) on {Interface} to CPU 0"))
    else:
        Results.append(("nic-irq", False, f"no IRQs pinned on {Interface}"))

    return Results


def TuneUDPMem():
    """Set UDP memory pressure thresholds (min/pressure/max pages)."""
    if not sys.platform.startswith("linux"):
        return [("udp_mem", False, "only available on Linux")]
    Results = []

    Key = "net/ipv4/udp_mem"
    PathValue = Path("/proc/sys") / Key
    Name = "net.ipv4.udp_mem"
    Value = "102400 873800 16777216"

    if not PathValue.is_file():
        return [(Name, False, "not available on this kernel")]

    try:
        Result = subprocess.run(
            WithPrivilege(["sysctl", "-w", f"{Name}={Value}"]),
            capture_output=True, text=True, timeout=10,
        )
        if Result.returncode == 0:
            Results.append((Name, True, f"set to {Value}"))
        else:
            Reason = (Result.stderr or Result.stdout).strip()
            Results.append((Name, False, Reason or "failed"))
    except (OSError, subprocess.SubprocessError) as Error:
        Results.append((Name, False, str(Error)))

    return Results


def TuneCpuLatency():
    """Write 0 to /dev/cpu_dma_latency to prevent deep C-states during playback."""
    if not sys.platform.startswith("linux"):
        return [("cpu_dma_latency", False, "only available on Linux")]

    LatencyFile = Path("/dev/cpu_dma_latency")
    if not LatencyFile.exists():
        return [("cpu_dma_latency", False, "not available")]

    try:
        # Open and keep the fd alive — closing it resets the latency
        # We store it on the function so it stays open for the process lifetime
        Fd = open(str(LatencyFile), "wb", buffering=0)
        Fd.write(b"\x00\x00\x00\x00")  # 0 microseconds = no deep C-states
        TuneCpuLatency._fd = Fd  # keep alive
        return [("cpu_dma_latency", True, "set to 0us (deep C-states blocked)")]
    except OSError as Error:
        return [("cpu_dma_latency", False, str(Error))]


def ClockSource():
    try:
        return Path("/sys/devices/system/clocksource/clocksource0/current_clocksource").read_text().strip()
    except OSError:
        return None


def Main():
    Parser = argparse.ArgumentParser(
        description="Play a local audio file in a Discord voice channel with VoicePlay."
    )
    Parser.add_argument("source", nargs="?", help="Local audio file path")
    Parser.add_argument("--guild", help="Guild ID override")
    Parser.add_argument("--channel", help="Voice channel ID override")
    Parser.add_argument("--token", action="append", help="Token override; repeat for multiple bots")
    Parser.add_argument("--ffmpeg", help="Path to the ffmpeg executable")
    Parser.add_argument("--voiceplay", help="Path to the VoicePlay executable")
    Parser.add_argument("--config", default=str(CONFIG_PATH), help="Path to config.json")
    Parser.add_argument("--dry-run", action="store_true", help="Print the command without launching VoicePlay")
    Parser.add_argument("--no-fq", action="store_true", help="Do not set up the fq qdisc or SO_TXTIME")
    Parser.add_argument("--no-tune", action="store_true", help="Do not apply kernel scheduler tweaks")
    Parser.add_argument("--no-net-tune", action="store_true", help="Do not apply network socket tuning")
    Parser.add_argument("--no-proc-tune", action="store_true", help="Do not apply ionice/renice to this process")
    Parser.add_argument("--no-irq-tune", action="store_true", help="Do not stop irqbalance or pin NIC IRQs")
    Parser.add_argument("--no-udp-mem", action="store_true", help="Do not tune net.ipv4.udp_mem")
    Parser.add_argument("--no-cstate", action="store_true", help="Do not block deep CPU C-states")
    Parser.add_argument("--fq-slack", default="1us", help="fq timer slack (default: 1us)")
    Parser.add_argument("--iface", help="Network interface for fq (default: auto-detect)")
    Mode = Parser.add_mutually_exclusive_group()
    Mode.add_argument("--selfbot", dest="selfbot", action="store_true", help="Use self-bot mode")
    Mode.add_argument("--bot", dest="selfbot", action="store_false", help="Use normal bot mode")
    Parser.set_defaults(selfbot=None)
    Arguments = Parser.parse_args()

    try:
        Config = LoadConfig(Path(Arguments.config).expanduser())
        Ffmpeg = ResolveFfmpeg(Config, Arguments.ffmpeg)
        if not Ffmpeg:
            raise RuntimeError(
                "ffmpeg is not installed or not on PATH. Install ffmpeg or set FFMPEG_PATH."
            )

        VoicePlay = ResolveVoicePlay(Arguments.voiceplay)
        if not VoicePlay:
            raise RuntimeError(
                "VoicePlay was not found. Build it or set VOICEPLAY_PATH."
            )

        Guild = Arguments.guild or os.environ.get("VOICE_GUILD_ID") or str(Config.get("guild_id") or "")
        Channel = Arguments.channel or os.environ.get("VOICE_CHANNEL_ID") or str(Config.get("channel_id") or "")
        if not Guild or not Channel:
            raise RuntimeError("Guild ID and channel ID are required.")

        SelfBot = Arguments.selfbot
        if SelfBot is None:
            SelfBot = bool(Config.get("selfbot", True))

        Source = Arguments.source
        if not Source:
            Candidate = Config.get("audio")
            if Candidate and Path(str(Candidate)).expanduser().is_file():
                Source = str(Candidate)
            else:
                Source = input("Audio path: ").strip()
        if not Source:
            raise RuntimeError("No audio file provided.")

        Audio = str(Path(Source).expanduser())
        if not Path(Audio).is_file():
            raise RuntimeError(f"Audio file not found: {Audio}")

        Tokens = ResolveTokens(Arguments, Config)

        Command = [VoicePlay]
        if SelfBot:
            Command.append("--selfbot")
        Command.extend([Guild, Channel, Audio])
        Command.extend(Tokens)

        Display = [VoicePlay]
        if SelfBot:
            Display.append("--selfbot")
        Display.extend([Guild, Channel, Audio])
        Display.extend(["***" for _ in Tokens])
        if os.name == "nt":
            Shown = subprocess.list2cmdline(Display)
        else:
            Shown = shlex.join(Display)
        print(f"Running: {Shown}")

        if Arguments.dry_run:
            print("[FQ] skipped (dry run)")
            return 0

        TxTime = False
        if Arguments.no_fq:
            print("[FQ] OFF - disabled with --no-fq")
        else:
            Enabled, Message = SetupFq(Arguments.iface, Arguments.fq_slack)
            print(f"[FQ] {'ON ' if Enabled else 'OFF'} - {Message}")
            TxTime = Enabled

        if Arguments.no_tune:
            print("[TUNE] OFF - disabled with --no-tune")
        else:
            for Name, Applied, Message in TuneKernel():
                print(f"[TUNE] {'ON ' if Applied else 'OFF'} - {Name}: {Message}")
            for Name, Applied, Message in ApplySysctlTunes(VM_TUNES, "vm tuning"):
                print(f"[VM] {'ON ' if Applied else 'OFF'} - {Name}: {Message}")
            for Name, Applied, Message in ApplySysctlTunes(FS_TUNES, "fs tuning"):
                print(f"[FS] {'ON ' if Applied else 'OFF'} - {Name}: {Message}")
            for Name, Applied, Message in ApplySysctlTunes(TCP_TUNES, "tcp tuning"):
                print(f"[TCP] {'ON ' if Applied else 'OFF'} - {Name}: {Message}")
            for Name, Applied, Message in ApplySysctlTunes(WRITEBACK_TUNES, "writeback tuning"):
                print(f"[WB] {'ON ' if Applied else 'OFF'} - {Name}: {Message}")
            Clock = ClockSource()
            if Clock:
                print(f"[TUNE] clocksource: {Clock}")

        if Arguments.no_net_tune:
            print("[NET] OFF - disabled with --no-net-tune")
        else:
            for Name, Applied, Message in TuneNetwork():
                print(f"[NET] {'ON ' if Applied else 'OFF'} - {Name}: {Message}")

        if Arguments.no_proc_tune:
            print("[PROC] OFF - disabled with --no-proc-tune")
        else:
            for Name, Applied, Message in TuneProcess():
                print(f"[PROC] {'ON ' if Applied else 'OFF'} - {Name}: {Message}")

        if Arguments.no_irq_tune:
            print("[IRQ] OFF - disabled with --no-irq-tune")
        else:
            for Name, Applied, Message in TuneIRQ(Arguments.iface):
                print(f"[IRQ] {'ON ' if Applied else 'OFF'} - {Name}: {Message}")

        if Arguments.no_udp_mem:
            print("[UDPMEM] OFF - disabled with --no-udp-mem")
        else:
            for Name, Applied, Message in TuneUDPMem():
                print(f"[UDPMEM] {'ON ' if Applied else 'OFF'} - {Name}: {Message}")

        if Arguments.no_cstate:
            print("[CSTATE] OFF - disabled with --no-cstate")
        else:
            for Name, Applied, Message in TuneCpuLatency():
                print(f"[CSTATE] {'ON ' if Applied else 'OFF'} - {Name}: {Message}")

        if TxTime and not IsRoot() and not shutil.which("sudo"):
            print("[TXTIME] OFF - needs root (CAP_NET_ADMIN) and sudo was not found")
            TxTime = False

        Environment = os.environ.copy()
        Environment["FFMPEG_PATH"] = Ffmpeg
        Environment["VOICE_TXTIME"] = "1" if TxTime else "0"
        print(f"[TXTIME] {'ON ' if TxTime else 'OFF'} - VOICE_TXTIME={Environment['VOICE_TXTIME']}")

        if TxTime and not IsRoot():
            Passed = ["FFMPEG_PATH", "VOICE_TXTIME"] + [
                Key for Key in Environment if Key.startswith("VOICE_") and Key != "VOICE_TXTIME"
            ]
            Command = ["sudo", "env"] + [f"{Key}={Environment[Key]}" for Key in Passed] + Command

        Result = subprocess.run(Command, cwd=str(SCRIPT_DIR), env=Environment)
        return Result.returncode
    except Exception as Error:
        print(f"error: {Error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(Main())
