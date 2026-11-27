import Foundation

struct XboxConsole: Decodable, Identifiable, Equatable {
    let id: String
    let name: String
    let model: String
    let power: String
}

/// A game a library grid shows: its cover, or while that is missing the same
/// letter, monogram and hue as on SteamOS, all from the core.
protocol LibraryItem: Identifiable where ID == String {
    var name: String { get }
    var letter: String { get }
    var monogram: String { get }
    var hue: Double { get }
    /// Whether a cover is on its way; false shows the placeholder for good.
    var hasCover: Bool { get }
    func cover(width: Int) -> URL?
}

/// A cloud game in the core's letter order.
struct CloudGame: Decodable, LibraryItem, Equatable {
    let id: String
    let name: String
    let cover: URL?
    let letter: String
    let monogram: String
    let hue: Double

    var hasCover: Bool { cover != nil }

    /// The cover `width` pixels wide: the Microsoft Store scales it on request.
    func cover(width: Int) -> URL? {
        guard let cover, var parts = URLComponents(url: cover, resolvingAgainstBaseURL: false) else { return nil }
        parts.queryItems = [URLQueryItem(name: "w", value: "\(width)"),
                            URLQueryItem(name: "h", value: "\(width * 3 / 2)")]
        return parts.url
    }
}

/// A PC with Sunshine as the core found it.
struct PcComputer: Decodable, Identifiable, Equatable {
    enum State: String, Decodable { case paired, unpaired, offline }
    let id: String
    let name: String
    /// Set for an unpaired PC: pairing asks for it.
    let address: String?
    let state: State
}

/// An app of a paired PC, in the core's letter order.
struct PcGame: Decodable, LibraryItem, Equatable {
    let id: String
    let name: String
    /// The ID of its PC.
    let pc: String
    let running: Bool
    /// The cover the PC keeps, once the core has it on disk.
    let cover: String?
    let letter: String
    let monogram: String
    let hue: Double

    var hasCover: Bool { cover != nil }
    func cover(width: Int) -> URL? { cover.map { URL(fileURLWithPath: $0) } }
}

enum CoreEvent {
    case device(url: URL?, code: String, prefilled: Bool)
    case account
    case signedOut
    case error(String)
    case consoles([XboxConsole])
    case cloud(games: [CloudGame], cached: Bool)
    case status(String)
    case ready(stun: [String], heartbeatMs: UInt64)
    case done
    case pcs(computers: [PcComputer], games: [PcGame])
    case pcPin(pin: String, name: String)
    case pcPaired(name: String)
    case pcStreaming(session: UInt64, hevc: Bool, hdr: Bool)
    case pcEnded(session: UInt64, error: String?)
    case pcError(String)
}

struct Handshake {
    struct Message {
        let label: String
        let json: String
    }
    let open: String
    let messages: [Message]
}

enum CoreError: Error, Equatable {
    case message(String)
}

/// Swift face of the portable Rust protocol core (`ios-core` crate).
///
/// All catalogue/stream calls are asynchronous: the Rust side spawns a worker
/// and reports progress through `onEvent`, which is delivered on the main
/// queue. SDP/ICE helpers are synchronous because they only touch the active
/// session.
final class PlaywakeCore {
    static let shared = PlaywakeCore()

    static let version: String = {
        guard let pointer = pw_core_version() else { return "unknown" }
        return String(cString: pointer)
    }()

    private let events = DispatchQueue(label: "io.playwake.core.events")
    private var handler: ((CoreEvent) -> Void)?

    var onEvent: ((CoreEvent) -> Void)? {
        get { events.sync { handler } }
        set { events.sync { handler = newValue } }
    }

    private init() {
        let documents = FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
        let session = documents.appendingPathComponent("playwake-session.json").path
        let storage = documents.appendingPathComponent("playwake-storage").path
        try? FileManager.default.createDirectory(atPath: storage, withIntermediateDirectories: true)
        if pw_init(session) != 0 {
            NSLog("playwake-core: pw_init failed")
        }
    }

    // MARK: - Account

    func restore() { pw_restore(trampoline) }
    func signIn() { pw_sign_in(trampoline) }
    func cancelSignIn() { pw_cancel_sign_in() }
    func signOut() { pw_sign_out(trampoline) }

    // MARK: - Catalogues

    func consoles() { pw_consoles(trampoline) }
    func cloudCatalog() { pw_cloud_catalog(trampoline) }

    // MARK: - Streaming

    func streamHome(console index: Int, height: UInt32) {
        pw_stream_home(trampoline, index, height)
    }

    func streamCloud(game id: String, height: UInt32) {
        pw_stream_cloud(trampoline, id, height)
    }

    // MARK: - PCs

    func pcCatalog() { pw_pc_catalog(trampoline) }
    func pairPc(address: String) { pw_pc_pair(trampoline, address) }
    func addPc(address: String) { pw_pc_add(trampoline, address) }

    /// The stream's session, or 0 when it could not start.
    func streamPc(game id: String, width: UInt32, height: UInt32, fps: UInt32, bitrateKbps: UInt32,
                  video: pw_pc_video, audio: pw_pc_audio) -> UInt64 {
        pw_pc_stream(trampoline, id, width, height, fps, bitrateKbps, false, video, audio)
    }

    /// The positions of `names` that match `query`, best first.
    func search(_ names: [String], query: String) -> [Int] {
        guard let data = try? JSONEncoder().encode(names),
              let raw = pw_library_search(String(decoding: data, as: UTF8.self), query) else { return [] }
        defer { pw_free_string(raw) }
        return (try? JSONDecoder().decode([Int].self, from: Data(String(cString: raw).utf8))) ?? []
    }

    func stop() { pw_stop() }
    func keepalive() { pw_keepalive() }

    func answer(offer: String) -> Result<String, CoreError> {
        guard let raw = pw_sdp(offer) else { return .failure(.message("No answer")) }
        defer { pw_free_string(raw) }
        let object = Self.object(raw)
        if let error = object?["error"] as? String { return .failure(.message(error)) }
        guard let sdp = object?["sdp"] as? String else { return .failure(.message("Malformed answer")) }
        return .success(sdp)
    }

    struct RemoteCandidate {
        let candidate: String
        let mid: String?
        let line: Int?
    }

    func exchange(candidates: [[String: Any]]) -> Result<[RemoteCandidate], CoreError> {
        guard let data = try? JSONSerialization.data(withJSONObject: candidates),
              let text = String(data: data, encoding: .utf8),
              let raw = pw_ice(text) else {
            return .failure(.message("Invalid candidates"))
        }
        defer { pw_free_string(raw) }
        let object = Self.object(raw)
        if let error = object?["error"] as? String { return .failure(.message(error)) }
        let list = object?["candidates"] as? [[String: Any]] ?? []
        return .success(list.compactMap { entry in
            guard let candidate = entry["candidate"] as? String else { return nil }
            return RemoteCandidate(
                candidate: candidate,
                mid: entry["sdpMid"] as? String,
                line: entry["sdpMLineIndex"] as? Int)
        })
    }

    // MARK: - Media helpers

    func mungeOffer(_ sdp: String, bitrate: UInt32, captureTime: Bool) -> (sdp: String, captureExtension: UInt8?)? {
        guard let raw = pw_munge_offer(sdp, bitrate, captureTime ? 1 : 0) else { return nil }
        defer { pw_free_string(raw) }
        guard let object = Self.object(raw), let text = object["sdp"] as? String else { return nil }
        let capture = (object["captureExtension"] as? NSNumber).map { UInt8(truncatingIfNeeded: $0.intValue) }
        return (text, capture)
    }

    func handshake(width: UInt32, height: UInt32) -> Handshake? {
        guard let raw = pw_handshake(width, height) else { return nil }
        defer { pw_free_string(raw) }
        guard let object = Self.object(raw),
              let open = object["open"] as? String,
              let messages = object["messages"] as? [[String: Any]] else { return nil }
        return Handshake(open: open, messages: messages.compactMap { entry in
            guard let label = entry["label"] as? String, let json = entry["json"] as? String else { return nil }
            return Handshake.Message(label: label, json: json)
        })
    }

    func isHandshakeAck(_ json: String) -> Bool {
        pw_is_handshake_ack(json) != 0
    }

    func connectPacket(startTime: Double) -> Data {
        var buffer = [UInt8](repeating: 0, count: pw_connect_packet(startTime, nil, 0))
        let size = pw_connect_packet(startTime, &buffer, buffer.count)
        return Data(buffer.prefix(size))
    }

    func inputPacket(sequence: UInt32, time: Double, buttons: UInt16,
                     left: (Int16, Int16), right: (Int16, Int16),
                     triggers: (UInt16, UInt16)) -> Data {
        var buffer = [UInt8](repeating: 0, count: pw_input_packet(
            sequence, time, buttons, left.0, left.1, right.0, right.1,
            triggers.0, triggers.1, nil, 0))
        let size = pw_input_packet(
            sequence, time, buttons, left.0, left.1, right.0, right.1,
            triggers.0, triggers.1, &buffer, buffer.count)
        return Data(buffer.prefix(size))
    }

    // MARK: - Events

    fileprivate func ingest(_ json: String) {
        guard let object = Self.object(json), let name = object["event"] as? String else { return }
        let event: CoreEvent?
        switch name {
        case "device":
            event = .device(
                url: (object["url"] as? String).flatMap(URL.init(string:)),
                code: object["code"] as? String ?? "",
                prefilled: object["prefilled"] as? Bool ?? false)
        case "account":
            event = .account
        case "signedOut":
            event = .signedOut
        case "error":
            event = .error(object["message"] as? String ?? "Unknown error")
        case "consoles":
            event = .consoles(Self.decode([XboxConsole].self, from: object["consoles"]) ?? [])
        case "cloud":
            event = .cloud(
                games: Self.decode([CloudGame].self, from: object["games"]) ?? [],
                cached: object["cached"] as? Bool ?? false)
        case "status":
            event = .status(object["message"] as? String ?? "")
        case "ready":
            event = .ready(
                stun: object["stun"] as? [String] ?? [],
                heartbeatMs: (object["heartbeatMs"] as? NSNumber)?.uint64Value ?? 0)
        case "done":
            event = .done
        case "pcs":
            event = .pcs(computers: Self.decode([PcComputer].self, from: object["pcs"]) ?? [],
                         games: Self.decode([PcGame].self, from: object["games"]) ?? [])
        case "pcPin":
            event = .pcPin(pin: object["pin"] as? String ?? "", name: object["name"] as? String ?? "")
        case "pcPaired":
            event = .pcPaired(name: object["name"] as? String ?? "")
        case "pcStreaming":
            event = .pcStreaming(session: (object["session"] as? NSNumber)?.uint64Value ?? 0,
                                 hevc: object["hevc"] as? Bool ?? false, hdr: object["hdr"] as? Bool ?? false)
        case "pcEnded":
            event = .pcEnded(session: (object["session"] as? NSNumber)?.uint64Value ?? 0,
                             error: object["error"] as? String)
        case "pcError":
            event = .pcError(object["message"] as? String ?? "Unknown error")
        default:
            event = nil
        }
        guard let event else { return }
        let handler = events.sync { self.handler }
        DispatchQueue.main.async { handler?(event) }
    }

    private static func object(_ raw: UnsafePointer<CChar>) -> [String: Any]? {
        object(String(cString: raw))
    }

    private static func object(_ json: String) -> [String: Any]? {
        guard let data = json.data(using: .utf8) else { return nil }
        return (try? JSONSerialization.jsonObject(with: data)) as? [String: Any]
    }

    private static func decode<T: Decodable>(_ type: T.Type, from value: Any?) -> T? {
        guard let value, JSONSerialization.isValidJSONObject(value),
              let data = try? JSONSerialization.data(withJSONObject: value) else { return nil }
        return try? JSONDecoder().decode(type, from: data)
    }
}

private func trampoline(_ json: UnsafePointer<CChar>?) {
    guard let json else { return }
    PlaywakeCore.shared.ingest(String(cString: json))
}
