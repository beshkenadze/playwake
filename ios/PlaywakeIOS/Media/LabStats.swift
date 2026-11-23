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
    var jitterBufferMs = 0.0
    var rttMs = 0.0
    var latencyMs = 0.0
    var framesDropped = 0
    var decodable = "waiting"

    var summary: String {
        String(
            format: "%dx%d  dec %.1f  ren %.1f  enc %.1f  %.1f Mb/s\njb %.1f ms  e2e %.1f ms  dec %.1f ms  jit %.1f ms  rtt %.1f ms  drop %d",
            width, height, decodeFps, renderFps, encodeFps, bitrateMbps,
            jitterBufferMs, latencyMs, decodeMs, jitterMs, rttMs, framesDropped)
    }
}

enum LabLog {
    private static let queue = DispatchQueue(label: "io.playwake.lab.log")
    private static let handle: FileHandle? = {
        let dir = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let url = dir.appendingPathComponent("lab.log")
        if !FileManager.default.fileExists(atPath: url.path) {
            FileManager.default.createFile(atPath: url.path, contents: nil)
        }
        let handle = try? FileHandle(forWritingTo: url)
        handle?.seekToEndOfFile()
        return handle
    }()

    static func write(_ line: String) {
        print("[lab] \(line)")
        queue.async {
            guard let data = (line + "\n").data(using: .utf8) else { return }
            try? handle?.write(contentsOf: data)
        }
    }
}
