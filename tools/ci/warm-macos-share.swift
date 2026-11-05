// Prime ShareKit/IconServices before native browser tests. On a cold macOS CI VM the first
// AppKit menu population (Chrome's Share menu, triggered by focus/keyboard work) blocks the
// browser's main thread in a synchronous IconServices XPC call for minutes. Observed via a
// process sample of Chrome 157 on macos-26-intel; this makes that cost land here, bounded.
import AppKit

let start = Date()
let services = NSSharingService.sharingServices(forItems: [URL(string: "https://example.com")!])
print("warm: \(services.count) sharing services in \(String(format: "%.1f", Date().timeIntervalSince(start)))s")
