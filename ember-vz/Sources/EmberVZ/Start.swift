import ArgumentParser
import Foundation
import Virtualization

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
        // Build the VM configuration (does not require main actor)
        let vmConfig = try buildConfiguration()

        // Schedule VM creation and start on the main queue.
        // VZVirtualMachine must be used from the queue it was created on
        // (defaults to main queue).
        DispatchQueue.main.async {
            do {
                try self.startVM(config: vmConfig)
            } catch {
                fputs("error: \(error.localizedDescription)\n", stderr)
                Darwin.exit(1)
            }
        }

        // Block forever on the main run loop. The VM delegate calls exit()
        // when the guest shuts down or an error occurs.
        dispatchMain()
    }

    // MARK: - VM Configuration

    /// Build the VZVirtualMachineConfiguration from CLI arguments.
    /// This assembles the boot loader, disk, network, and console devices.
    private func buildConfiguration() throws -> VZVirtualMachineConfiguration {
        let config = VZVirtualMachineConfiguration()

        // Boot loader: direct Linux kernel boot (like Firecracker)
        let bootLoader = VZLinuxBootLoader(
            kernelURL: URL(fileURLWithPath: kernel)
        )
        bootLoader.commandLine = bootArgs
        config.bootLoader = bootLoader

        // CPU and memory
        config.cpuCount = cpus
        config.memorySize = UInt64(memory) * 1024 * 1024

        // Storage: raw ext4 disk image as virtio-blk (/dev/vda in guest)
        let diskAttachment = try VZDiskImageStorageDeviceAttachment(
            url: URL(fileURLWithPath: disk),
            readOnly: false,
            cachingMode: .cached,
            synchronizationMode: .full
        )
        config.storageDevices = [
            VZVirtioBlockDeviceConfiguration(attachment: diskAttachment)
        ]

        // Network: vmnet shared mode (NAT + DHCP, no root required)
        let networkDevice = VZVirtioNetworkDeviceConfiguration()
        networkDevice.attachment = VZNATNetworkDeviceAttachment()
        networkDevice.macAddress = VZMACAddress.randomLocallyAdministered()
        config.networkDevices = [networkDevice]

        // Serial console: virtio-console device.
        // Guest output goes to stdout (or a log file); guest input from /dev/null.
        let serialPort = VZVirtioConsoleDeviceSerialPortConfiguration()
        let outputHandle: FileHandle
        if let logPath = serialLog {
            // Ensure the log file exists so we can open it for writing
            FileManager.default.createFile(atPath: logPath, contents: nil)
            guard let handle = FileHandle(forWritingAtPath: logPath) else {
                throw ValidationError("Cannot open serial log file: \(logPath)")
            }
            handle.seekToEndOfFile()
            outputHandle = handle
        } else {
            outputHandle = FileHandle.standardOutput
        }
        serialPort.attachment = VZFileHandleSerialPortAttachment(
            fileHandleForReading: FileHandle.nullDevice,
            fileHandleForWriting: outputHandle
        )
        config.serialPorts = [serialPort]

        // Entropy: virtio-rng provides /dev/urandom in guest
        config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

        // Memory balloon: allows host to reclaim unused guest memory
        config.memoryBalloonDevices = [
            VZVirtioTraditionalMemoryBalloonDeviceConfiguration()
        ]

        try config.validate()
        return config
    }

    // MARK: - VM Lifecycle

    /// Create the VM from a validated configuration, start it, and install
    /// a delegate that exits the process when the guest shuts down.
    @MainActor
    private func startVM(config: VZVirtualMachineConfiguration) throws {
        let vm = VZVirtualMachine(configuration: config)

        // Capture MAC address for reporting after boot
        let mac = config.networkDevices.first!.macAddress.string
        fputs("MAC=\(mac)\n", stderr)

        // Delegate handles guest stop / error → process exit
        let delegate = VMDelegate()
        vm.delegate = delegate

        // Keep a strong reference to the delegate and VM so they aren't deallocated
        _vmRef = vm
        _delegateRef = delegate

        // Install SIGTERM handler: graceful shutdown via VZVirtualMachine.stop().
        // We must ignore the default SIGTERM behavior first, then use a DispatchSource
        // to receive the signal and call stop() on the main queue.
        signal(SIGTERM, SIG_IGN)
        let sigtermSource = DispatchSource.makeSignalSource(signal: SIGTERM, queue: .main)
        sigtermSource.setEventHandler {
            fputs("received SIGTERM, stopping VM gracefully...\n", stderr)
            guard let vm = _vmRef else { Darwin.exit(0) }
            vm.stop { error in
                if let error = error {
                    fputs("warning: graceful stop failed: \(error.localizedDescription)\n", stderr)
                }
                // The delegate's guestDidStop will handle exit, but if stop() fails
                // we exit here as a fallback.
                Darwin.exit(0)
            }
        }
        sigtermSource.resume()
        _sigtermSourceRef = sigtermSource

        // Install SIGUSR1 handler: pause the VM.
        signal(SIGUSR1, SIG_IGN)
        let sigusr1Source = DispatchSource.makeSignalSource(signal: SIGUSR1, queue: .main)
        sigusr1Source.setEventHandler {
            guard let vm = _vmRef else { return }
            guard vm.canPause else {
                fputs("warning: VM cannot be paused in current state\n", stderr)
                return
            }
            fputs("received SIGUSR1, pausing VM...\n", stderr)
            vm.pause { result in
                switch result {
                case .success:
                    fputs("vm paused\n", stderr)
                case .failure(let error):
                    fputs("warning: pause failed: \(error.localizedDescription)\n", stderr)
                }
            }
        }
        sigusr1Source.resume()
        _sigusr1SourceRef = sigusr1Source

        // Install SIGUSR2 handler: resume the VM.
        signal(SIGUSR2, SIG_IGN)
        let sigusr2Source = DispatchSource.makeSignalSource(signal: SIGUSR2, queue: .main)
        sigusr2Source.setEventHandler {
            guard let vm = _vmRef else { return }
            guard vm.canResume else {
                fputs("warning: VM cannot be resumed in current state\n", stderr)
                return
            }
            fputs("received SIGUSR2, resuming VM...\n", stderr)
            vm.resume { result in
                switch result {
                case .success:
                    fputs("vm resumed\n", stderr)
                case .failure(let error):
                    fputs("warning: resume failed: \(error.localizedDescription)\n", stderr)
                }
            }
        }
        sigusr2Source.resume()
        _sigusr2SourceRef = sigusr2Source

        // Capture ready-fd for use in the start callback
        let readyFd = self.readyFd

        vm.start { result in
            switch result {
            case .success:
                fputs("vm started\n", stderr)

                // Report MAC address to ready-fd now that the VM is booted.
                // The parent process (ember) reads this to discover the guest IP
                // via DHCP lease matching.
                if let fd = readyFd {
                    let readyHandle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
                    readyHandle.write(Data("\(mac)\n".utf8))
                }

            case .failure(let error):
                fputs("error: vm failed to start: \(error.localizedDescription)\n", stderr)
                Darwin.exit(1)
            }
        }
    }
}

// Strong references to keep the VM, delegate, and signal sources alive
// for the process lifetime. These live at module scope since dispatchMain()
// never returns.
nonisolated(unsafe) var _vmRef: VZVirtualMachine?
nonisolated(unsafe) var _delegateRef: VMDelegate?
nonisolated(unsafe) var _sigtermSourceRef: DispatchSourceSignal?
nonisolated(unsafe) var _sigusr1SourceRef: DispatchSourceSignal?
nonisolated(unsafe) var _sigusr2SourceRef: DispatchSourceSignal?

// MARK: - VM Delegate

/// Handles VM lifecycle events. Exits the process when the guest stops.
class VMDelegate: NSObject, VZVirtualMachineDelegate {
    func guestDidStop(_ virtualMachine: VZVirtualMachine) {
        fputs("vm stopped\n", stderr)
        exit(0)
    }

    func virtualMachine(
        _ virtualMachine: VZVirtualMachine,
        didStopWithError error: any Error
    ) {
        fputs("error: vm stopped unexpectedly: \(error.localizedDescription)\n", stderr)
        exit(1)
    }
}
