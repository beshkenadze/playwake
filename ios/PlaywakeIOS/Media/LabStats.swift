import Foundation

struct LabStats: Equatable {
    var state = "idle"
    var width = 0
    var height = 0
    var decodeFps = 0.0
    var renderFps = 0.0
    var encodeFps = 0.0
    var bitrateMbps = 0.0
    var jitterMs = 0.0
    var decodeMs = 0.0
    var encodeMs = 0.0
    var jitterBufferMs = 0.0
    var submitMs = 0.0
    var presentMs = 0.0
    var holdMs = 0.0
    /// Network type of the selected ICE pair ("ethernet", "wifi", ...).
    var network = ""
    var rttMs = 0.0
    var latencyMs = 0.0
    var framesDropped = 0
    var decodable = "waiting"

    /// Client share of the latency: jitter buffer, decode, hand-off to the
    /// renderer, enqueue and display.
    var clientMs: Double { jitterBufferMs + decodeMs + holdMs + submitMs + presentMs }

    var summary: String {
        String(
            format: "%dx%d  dec %.1f  ren %.1f  enc %.1f  %.1f Mb/s\njb %.1f ms  hold %.2f ms  sub %.2f ms  pres %.2f ms  e2e %.2f ms  dec %.1f ms  encT %.1f ms  jit %.1f ms  rtt %.1f ms  drop %d",
            width, height, decodeFps, renderFps, encodeFps, bitrateMbps,
            jitterBufferMs, holdMs, submitMs, presentMs, latencyMs, decodeMs, encodeMs, jitterMs, rttMs, framesDropped)
    }
}

/// Debug-only on-device log. Release builds compile the writes out, so the
/// app does not touch the disk; production wants `SessionEvents` instead.
///
/// The file lives in Documents and is rotated at ``maxBytes``: `lab.log`
/// becomes `lab.log.1` (replacing the previous one) and a fresh file starts,
/// so at most ~4 MiB is kept on disk.
enum LabLog {
    #if DEBUG
    private static let queue = DispatchQueue(label: "io.playwake.lab.log")
    private static let maxBytes = 2 * 1024 * 1024
    private static let directory = FileManager.default.urls(
        for: .documentDirectory, in: .userDomainMask)[0]
    private static var url: URL { directory.appendingPathComponent("lab.log") }
    private static var previousURL: URL { directory.appendingPathComponent("lab.log.1") }
    private static var handle: FileHandle?
    private static var size = 0
    #endif

    static func write(_ line: @autoclosure () -> String) {
        #if DEBUG
        let text = line()
        print("[lab] \(text)")
        guard let data = (text + "\n").data(using: .utf8) else { return }
        queue.async {
            rotateIfNeeded(appending: data.count)
            try? handle?.write(contentsOf: data)
            size += data.count
        }
        #endif
    }

    #if DEBUG
    private static func rotateIfNeeded(appending bytes: Int) {
        if handle == nil {
            open()
        }
        guard size + bytes > maxBytes else { return }
        try? handle?.close()
        handle = nil
        try? FileManager.default.removeItem(at: previousURL)
        try? FileManager.default.moveItem(at: url, to: previousURL)
        size = 0
        open()
    }

    private static func open() {
        if !FileManager.default.fileExists(atPath: url.path) {
            FileManager.default.createFile(atPath: url.path, contents: nil)
        }
        guard let file = try? FileHandle(forWritingTo: url) else { return }
        file.seekToEndOfFile()
        if let attributes = try? FileManager.default.attributesOfItem(atPath: url.path),
           let fileSize = attributes[.size] as? NSNumber {
            size = fileSize.intValue
        } else {
            size = 0
        }
        handle = file
    }
    #endif
}
