#!/usr/bin/env python3
"""
Run e2e test cases locally, mimicking the GitHub Actions workflow.

Usage:
    python run_e2e_local.py <workflow> [--test-env <env>] [--extra-bru-args <args>]
    python run_e2e_local.py 3-nodes-transfer
    python run_e2e_local.py cross-chain-hub --test-env test
    python run_e2e_local.py funding-tx-verification --extra-bru-args "--env-var FUNDING_TX_VERIFICATION_CASE=remove_change"
"""

import argparse
import os
import signal
import subprocess
import sys
import threading
import time
from pathlib import Path
from typing import List, Optional


class E2ERunner:
    def __init__(self, workflow: str, test_env: str = "test", extra_bru_args: str = ""):
        self.workflow = workflow
        self.test_env = test_env
        self.extra_bru_args = extra_bru_args
        self.workflow_path = f"e2e/{workflow}"
        self.project_root = Path(__file__).absolute().parent.parent.parent.parent
        self.nodes_dir = self.project_root / "tests" / "nodes"
        self.bruno_dir = self.project_root / "tests" / "bruno"
        self.workflow_dir = self.bruno_dir / self.workflow_path
        self.background_processes: List[subprocess.Popen] = []
        self.cleanup_called = False  # Track if cleanup has been called
        self.output_threads: List[
            threading.Thread
        ] = []  # Threads reading process output
        self.start_sh_process: Optional[subprocess.Popen] = (
            None  # Track start.sh process
        )
        self.bruno_process: Optional[subprocess.Popen] = (
            None  # Track Bruno test process
        )
        self._setup_signal_handlers()

    def _setup_signal_handlers(self):
        """Set up signal handlers to ensure cleanup on interrupt."""

        def signal_handler(signum, frame):
            """Handle SIGINT (Ctrl-C) and SIGTERM."""
            if not self.cleanup_called:
                print("\n\nReceived interrupt signal, cleaning up...")
                self.cleanup()
            sys.exit(130 if signum == signal.SIGINT else 128 + signum)

        # Register handlers for SIGINT (Ctrl-C) and SIGTERM
        signal.signal(signal.SIGINT, signal_handler)
        signal.signal(signal.SIGTERM, signal_handler)

    def _kill_via_pid_file(self, pid_file_path: Path, name: str):
        """Kill a process (or processes) using a PID file."""
        try:
            if not pid_file_path.exists():
                return

            with open(pid_file_path, "r") as f:
                pid_lines = [line.strip() for line in f.readlines() if line.strip()]

            if not pid_lines:
                return

            for pid_str in pid_lines:
                try:
                    pid = int(pid_str)
                except ValueError:
                    continue

                try:
                    # Check if process exists
                    os.kill(pid, 0)
                except ProcessLookupError:
                    continue

                print(f"Killing {name} (PID: {pid})")
                try:
                    os.kill(pid, signal.SIGTERM)
                    time.sleep(1)
                    os.kill(pid, 0)
                    print(f"Force killing {name} (PID: {pid})")
                    os.kill(pid, signal.SIGKILL)
                except ProcessLookupError:
                    continue

            # Remove PID file after attempting to kill processes
            try:
                pid_file_path.unlink()
            except OSError:
                pass
        except (OSError, IOError):
            # File missing or unreadable
            pass

    def check_prerequisites(self):
        """Check if required directories and tools exist."""
        if not self.workflow_dir.exists():
            print(
                f"Error: Workflow directory not found: {self.workflow_dir}",
                file=sys.stderr,
            )
            sys.exit(1)

        # Check for required tools
        for tool in ["cargo", "npm", "nc"]:
            if not self._command_exists(tool):
                print(
                    f"Error: Required tool '{tool}' not found in PATH", file=sys.stderr
                )
                sys.exit(1)

    def _command_exists(self, command: str) -> bool:
        """Check if a command exists in PATH."""
        return (
            subprocess.run(
                ["which", command], capture_output=True, check=False
            ).returncode
            == 0
        )

    def _stream_output(self, process: subprocess.Popen, prefix: str = ""):
        """Stream output from a process to stdout in real-time."""
        if process.stdout is None:
            return

        def read_output():
            try:
                for line in iter(process.stdout.readline, ""):
                    if line:
                        if prefix:
                            print(f"[{prefix}] {line.rstrip()}", flush=True)
                        else:
                            print(line.rstrip(), flush=True)
            except (ValueError, OSError):
                # Process closed stdout or terminated
                pass

        thread = threading.Thread(target=read_output, daemon=True)
        thread.start()
        self.output_threads.append(thread)

    def run_command(
        self,
        cmd: List[str],
        cwd: Optional[Path] = None,
        env: Optional[dict] = None,
        background: bool = False,
        check: bool = True,
        output_prefix: str = "",
    ) -> Optional[subprocess.Popen]:
        """Run a command and optionally wait for it."""
        print(f"Running: {' '.join(cmd)}")
        if cwd:
            print(f"  in directory: {cwd}")

        process_env = os.environ.copy()
        if env:
            process_env.update(env)

        # Set stdin to DEVNULL to prevent processes from waiting for input
        process = subprocess.Popen(
            cmd,
            cwd=cwd or self.project_root,
            env=process_env,
            stdin=subprocess.DEVNULL,  # Prevent processes from waiting for input
            stdout=subprocess.PIPE if background else None,
            stderr=subprocess.STDOUT if background else None,
            text=True,
            bufsize=1,  # Line buffered for real-time output
        )

        if background:
            self.background_processes.append(process)
            # Start streaming output in a separate thread
            if process.stdout:
                self._stream_output(process, output_prefix)
            return process
        else:
            if check:
                process.wait()
                if process.returncode != 0:
                    print(
                        f"Error: Command failed with exit code {process.returncode}",
                        file=sys.stderr,
                    )
                    sys.exit(process.returncode)
            return None

    def prebuild(self):
        """Prebuild the program and UDT init."""
        print("\n=== Prebuilding ===")

        # Build main program
        print("Building main program...")
        self.run_command(["cargo", "build", "--locked"])

        # Build UDT init
        print("Building UDT init...")
        udt_init_dir = self.project_root / "tests" / "deploy" / "udt-init"
        self.run_command(["cargo", "build", "--locked"], cwd=udt_init_dir)

    def start_nodes(self):
        """Start the test nodes in the background using start.sh."""
        print("\n=== Starting nodes ===")

        start_script = self.nodes_dir / "start.sh"
        if not start_script.exists():
            print(f"Error: start.sh not found at {start_script}", file=sys.stderr)
            sys.exit(1)

        # Delete .ports file if it exists (from previous runs)
        ports_file = self.nodes_dir / ".ports"
        try:
            ports_file.unlink()
            print("Deleted existing .ports file")
        except OSError:
            pass  # Ignore if deletion fails

        # Set environment variables as the workflow does
        # start.sh handles all special cases internally (cross-chain-hub, funding-tx-verification, etc.)
        env = os.environ.copy()
        env["EXTRA_BRU_ARGS"] = self.extra_bru_args
        env["REMOVE_OLD_STATE"] = "y"  # Remove old state for clean test runs
        # Set ON_GITHUB_ACTION only when running in CI (GitHub Actions)
        if os.environ.get("GITHUB_ACTIONS") == "true":
            env["ON_GITHUB_ACTION"] = "y"

        # Start nodes in background (start.sh handles all the logic)
        process = self.run_command(
            ["bash", str(start_script), self.workflow_path],
            background=True,
            check=False,
            env=env,
            output_prefix="start.sh",
        )

        # Track the start.sh process
        self.start_sh_process = process

        if process and process.returncode is not None and process.returncode != 0:
            print("Error: Failed to start nodes", file=sys.stderr)
            sys.exit(1)

    def wait_for_ports(self):
        """Wait for all required ports to be open."""
        print("\n=== Waiting for ports ===")

        wait_script = self.nodes_dir / "wait.sh"
        if not wait_script.exists():
            print(f"Error: wait.sh not found at {wait_script}", file=sys.stderr)
            sys.exit(1)

        self.run_command(["bash", str(wait_script)])

    def run_bruno_tests(self):
        """Run the Bruno CLI tests in the background (matching workflow behavior)."""
        print("\n=== Running Bruno tests ===")

        bruno_cmd = [
            "npm",
            "exec",
            "--yes",
            "--",
            "@usebruno/cli@1.20.0",
            "run",
            self.workflow_path,
            "-r",
            "--env",
            self.test_env,
        ]

        if self.extra_bru_args:
            # Split extra_bru_args into separate arguments
            bruno_cmd.extend(self.extra_bru_args.split())

        # Set environment to make npm exec non-interactive
        env = os.environ.copy()
        env["NPM_CONFIG_YES"] = "true"
        env["CI"] = "true"  # Some tools check CI to skip prompts

        # Run in background like the workflow does
        process = self.run_command(
            bruno_cmd,
            cwd=self.bruno_dir,
            background=True,
            check=False,
            output_prefix="bruno",
            env=env,
        )

        # Track the Bruno test process
        self.bruno_process = process

    def cleanup_ckb_process(self):
        """Clean up ckb process started by start.sh."""
        deploy_dir = self.project_root / "tests" / "deploy"
        ckb_pid_file = deploy_dir / "node-data" / "ckb.pid"

        # Try to kill via PID file first (if it exists)
        self._kill_via_pid_file(ckb_pid_file, "ckb")

        # Also try to find and kill ckb processes via pkill
        # This is a safety net in case PID file is missing
        try:
            subprocess.run(
                ["pkill", "-f", "ckb run.*node-data"],
                capture_output=True,
                timeout=2,
                check=False,
            )
        except (subprocess.TimeoutExpired, FileNotFoundError):
            # pkill might not be available or timeout
            pass

    def cleanup_fnn_processes(self):
        """Clean up fnn processes started by start.sh."""
        # Kill fnn processes with -d argument (nodes 1, 2, 3, or bootnode)
        try:
            subprocess.run(
                ["pkill", "-f", "fnn -d"], capture_output=True, timeout=2, check=False
            )
        except (subprocess.TimeoutExpired, FileNotFoundError):
            # pkill might not be available or timeout
            pass

    def cleanup_lnd_processes(self):
        """Clean up bitcoind and lnd processes started by setup-lnd.sh."""
        if self.workflow != "cross-chain-hub":
            return

        lnd_init_dir = self.project_root / "tests" / "deploy" / "lnd-init"

        # Kill bitcoind, lnd-bob, and lnd-ingrid via their PID files
        self._kill_via_pid_file(lnd_init_dir / "bitcoind" / "bitcoind.pid", "bitcoind")
        self._kill_via_pid_file(lnd_init_dir / "lnd-bob" / "lnd.pid", "lnd-bob")
        self._kill_via_pid_file(lnd_init_dir / "lnd-ingrid" / "lnd.pid", "lnd-ingrid")

        # Also try to find and kill any remaining bitcoind/lnd processes
        # This is a safety net in case PID files are missing
        try:
            # Kill bitcoind processes
            subprocess.run(
                ["pkill", "-f", "bitcoind.*lnd-init"],
                capture_output=True,
                timeout=2,
                check=False,
            )
            # Kill lnd processes
            subprocess.run(
                ["pkill", "-f", "lnd.*lnd-init"],
                capture_output=True,
                timeout=2,
                check=False,
            )
        except (subprocess.TimeoutExpired, FileNotFoundError):
            # pkill might not be available or timeout
            pass

    def cleanup(self):
        """Clean up background processes and all their descendants."""
        if self.cleanup_called:
            return  # Prevent double cleanup
        self.cleanup_called = True

        print("\n=== Cleaning up ===")

        # Clean up ckb process first
        self.cleanup_ckb_process()

        # Clean up fnn processes
        self.cleanup_fnn_processes()

        # Clean up bitcoind/lnd processes (for cross-chain-hub)
        self.cleanup_lnd_processes()

        for process in self.background_processes:
            if process.poll() is None:  # Process is still running
                print(f"Terminating process {process.pid}")
                try:
                    process.terminate()
                    process.wait(timeout=2)
                except (subprocess.TimeoutExpired, ProcessLookupError):
                    try:
                        process.kill()
                        process.wait(timeout=1)
                    except (subprocess.TimeoutExpired, ProcessLookupError):
                        pass

        # Final cleanup of ckb, fnn, and lnd processes (in case they weren't killed earlier)
        self.cleanup_ckb_process()
        self.cleanup_fnn_processes()
        self.cleanup_lnd_processes()

    def wait_for_background_processes(self):
        """Wait for background processes to exit and verify Bruno tests passed."""
        print("\n=== Waiting for background processes ===")
        if not self.background_processes:
            print("Error: No background processes to wait for", file=sys.stderr)
            return 1

        if not self.bruno_process:
            print("Error: Bruno test process not found", file=sys.stderr)
            return 1

        # Wait for any process to exit (like 'wait -n' in bash)
        while True:
            # Check if Bruno tests have completed
            if self.bruno_process and self.bruno_process.poll() is not None:
                bruno_exit_code = self.bruno_process.returncode
                if bruno_exit_code == 0:
                    print(
                        f"\nBruno tests completed successfully (exit code: {bruno_exit_code})"
                    )
                    # Bruno tests passed, we can exit successfully
                    return 0
                else:
                    print(
                        f"\nBruno tests failed with exit code: {bruno_exit_code}",
                        file=sys.stderr,
                    )
                    # Bruno tests failed, exit with that code
                    return bruno_exit_code

            # Check if start.sh has exited (this is a failure - nodes should keep running)
            if self.start_sh_process and self.start_sh_process.poll() is not None:
                start_exit_code = self.start_sh_process.returncode
                print(
                    f"\nError: start.sh exited unexpectedly (exit code: {start_exit_code})",
                    file=sys.stderr,
                )
                print(
                    "Nodes should keep running during tests. This indicates a problem.",
                    file=sys.stderr,
                )
                # start.sh exiting is always a failure
                return 1 if start_exit_code == 0 else start_exit_code

            # Check other background processes (shouldn't normally exit)
            for process in self.background_processes:
                if process != self.start_sh_process and process != self.bruno_process:
                    if process.poll() is not None:
                        exit_code = process.returncode
                        print(
                            f"\nError: Unexpected process {process.pid} exited with code {exit_code}",
                            file=sys.stderr,
                        )
                        return exit_code if exit_code != 0 else 1

            time.sleep(0.5)

    def run(self):
        """Run the complete e2e test workflow."""
        exit_code = 0
        try:
            self.check_prerequisites()
            self.prebuild()
            self.start_nodes()  # start.sh handles all special cases
            self.wait_for_ports()
            self.run_bruno_tests()  # Runs in background
            exit_code = (
                self.wait_for_background_processes()
            )  # Wait for Bruno tests to complete
            if exit_code == 0:
                print("\n=== Test completed successfully ===")
            else:
                print(
                    f"\n=== Test failed with exit code {exit_code} ===", file=sys.stderr
                )
        except KeyboardInterrupt:
            # Signal handler will handle cleanup, but ensure it's called here too
            if not self.cleanup_called:
                print("\n\nInterrupted by user")
                self.cleanup()
            exit_code = 130
        except SystemExit as e:
            # Don't cleanup on SystemExit if it's from our own sys.exit
            # The signal handler or finally block will handle it
            exit_code = e.code if e.code is not None else 1
            raise
        except Exception as e:
            print(f"\nError: {e}", file=sys.stderr)
            import traceback

            traceback.print_exc()
            if not self.cleanup_called:
                self.cleanup()
            exit_code = 1
        finally:
            # Ensure cleanup is always called, even if something unexpected happens
            if not self.cleanup_called:
                self.cleanup()
            # Exit with the appropriate code (0 only if Bruno tests passed)
            sys.exit(exit_code)


def main():
    parser = argparse.ArgumentParser(
        description="Run e2e test cases locally",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog="""
Examples:
  %(prog)s 3-nodes-transfer
  %(prog)s cross-chain-hub --test-env test
  %(prog)s funding-tx-verification --extra-bru-args "--env-var FUNDING_TX_VERIFICATION_CASE=remove_change"
  %(prog)s udt --test-env xudt-test
        """,
    )

    parser.add_argument(
        "workflow",
        help="Workflow name (e.g., '3-nodes-transfer', 'cross-chain-hub', 'udt')",
    )

    parser.add_argument(
        "--test-env", default="test", help="Test environment to use (default: 'test')"
    )

    parser.add_argument(
        "--extra-bru-args",
        default="",
        help="Extra arguments to pass to Bruno CLI (e.g., '--env-var KEY=value')",
    )

    args = parser.parse_args()

    runner = E2ERunner(
        workflow=args.workflow,
        test_env=args.test_env,
        extra_bru_args=args.extra_bru_args,
    )

    runner.run()


if __name__ == "__main__":
    main()
