import ArgumentParser

/// ember-vz: Swift helper for running Linux VMs via Apple Virtualization Framework.
///
/// This tool is invoked by the ember CLI — it is not intended to be called directly.
/// It manages the lifecycle of a single VM process, communicating status back to
/// ember via a ready-fd and responding to signals for stop/pause/resume.
@main
struct EmberVZ: ParsableCommand {
    static let configuration = CommandConfiguration(
        commandName: "ember-vz",
        abstract: "Manage Linux VMs using Apple Virtualization Framework",
        subcommands: [Start.self]
    )
}
