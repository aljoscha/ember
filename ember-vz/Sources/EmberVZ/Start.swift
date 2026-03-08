import ArgumentParser
import Foundation

/// Boot a Linux VM with the given kernel, disk, and configuration.
///
/// This command blocks until the VM exits or the process receives a signal.
/// Once the VM is booted, the guest MAC address is written to --ready-fd
/// so the parent process (ember) can discover the guest IP via DHCP leases.
///
/// Signal handling:
///   SIGTERM  → graceful shutdown (VZVirtualMachine.stop)
///   SIGKILL  → force stop (handled by OS)
///   SIGUSR1  → pause VM
///   SIGUSR2  → resume VM
struct Start: ParsableCommand {
    static let configuration = CommandConfiguration(
        abstract: "Boot a Linux VM via AVF"
    )

    @Option(help: "Path to vmlinux kernel image")
    var kernel: String

    @Option(help: "Path to root filesystem disk image")
    var disk: String

    @Option(help: "Number of virtual CPUs")
    var cpus: Int = 2

    @Option(help: "Memory size in megabytes")
    var memory: Int = 512

    @Option(name: .long, help: "Kernel boot arguments")
    var bootArgs: String = "console=hvc0 root=/dev/vda rw"

    @Option(help: "Network mode: 'shared' (vmnet NAT)")
    var network: String = "shared"

    @Option(name: .long, help: "Path to serial console log file")
    var serialLog: String? = nil

    @Option(name: .long, help: "File descriptor to write ready notification (MAC address)")
    var readyFd: Int32? = nil

    func run() throws {
        // TODO: Implement VM boot in subsequent tasks:
        //   1. Configure VZLinuxBootLoader with kernel + boot args
        //   2. Attach virtio-blk disk, virtio-net (shared vmnet), virtio-console
        //   3. Start VM, wait for boot, write MAC to ready-fd
        //   4. Install signal handlers for SIGTERM/SIGUSR1/SIGUSR2
        //   5. Run until VM exits or signal received
        print("ember-vz: start command not yet implemented")
        throw ExitCode.failure
    }
}
