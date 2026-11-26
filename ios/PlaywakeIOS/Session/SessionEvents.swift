import Combine
import Foundation

/// App-wide bus for session telemetry.
///
/// The streaming code only publishes here: debug builds also log each sample
/// to `lab.log`, while production builds have no disk logging and can attach
/// a telemetry sink instead. Stats are throttled to one sample per
/// ``statsInterval`` seconds so a production consumer sees a steady, cheap
/// heartbeat rather than a message every second.
@MainActor
final class SessionEvents {
    static let shared = SessionEvents()

    enum Event {
        case stats(LabStats)
    }

    /// Minimum spacing between published stats samples.
    static let statsInterval: TimeInterval = 5

    private let subject = PassthroughSubject<Event, Never>()
    private var lastStatsAt: CFAbsoluteTime = 0

    var publisher: AnyPublisher<Event, Never> { subject.eraseToAnyPublisher() }

    func publish(_ stats: LabStats) {
        let now = CFAbsoluteTimeGetCurrent()
        guard now - lastStatsAt >= Self.statsInterval else { return }
        lastStatsAt = now
        subject.send(.stats(stats))
    }
}
