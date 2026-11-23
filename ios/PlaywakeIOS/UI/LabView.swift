import SwiftUI

struct LabView: View {
    @ObservedObject var controller: LabController

    var body: some View {
        ZStack(alignment: .topLeading) {
            LowLatencyVideoView(renderer: controller.renderer)
                .ignoresSafeArea()
                .background(Color.black)

            VStack(alignment: .leading, spacing: 6) {
                HStack(spacing: 8) {
                    Circle()
                        .fill(controller.running ? Color.red : Color.gray)
                        .frame(width: 8, height: 8)
                    Text("PLAYWAKE · LOW-LATENCY LAB")
                        .font(.system(size: 11, weight: .bold, design: .monospaced))
                }
                Text(controller.stats.summary)
                    .font(.system(size: 12, weight: .medium, design: .monospaced))
                    .fixedSize(horizontal: false, vertical: true)
                Text(controller.stats.state)
                    .font(.system(size: 10, weight: .regular, design: .monospaced))
                    .foregroundStyle(.secondary)
            }
            .foregroundStyle(.white)
            .padding(10)
            .background(Color.black.opacity(0.55))
            .clipShape(RoundedRectangle(cornerRadius: 8))
            .padding(12)

            VStack {
                Spacer()
                HStack(spacing: 12) {
                    Button(action: { controller.running ? controller.stop() : controller.start() }) {
                        Label(controller.running ? "Stop" : "Start", systemImage: controller.running ? "stop.fill" : "play.fill")
                            .font(.system(size: 15, weight: .semibold, design: .monospaced))
                            .padding(.horizontal, 18)
                            .padding(.vertical, 12)
                            .background(Color.white.opacity(0.16))
                            .clipShape(Capsule())
                    }
                    Spacer()
                    Text("H.264 · 720p60 · loopback")
                        .font(.system(size: 11, weight: .medium, design: .monospaced))
                        .foregroundStyle(.white.opacity(0.7))
                }
                .padding(.horizontal, 16)
                .padding(.bottom, 18)
            }
        }
    }
}
