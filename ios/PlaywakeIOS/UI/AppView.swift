import SwiftUI

/// Top-level flow: restoring → welcome → sign-in → home, and the stream over
/// everything while it runs.
struct AppView: View {
    @ObservedObject var controller: SessionController
    @Environment(\.scenePhase) private var scenePhase

    var body: some View {
        ZStack {
            Theme.canvas.ignoresSafeArea()
            content
        }
        .tint(Theme.mint)
        .onAppear { controller.restore() }
        .onChange(of: scenePhase) { _, phase in
            if phase == .background && controller.streaming {
                controller.stop()
            }
        }
    }

    @ViewBuilder
    private var content: some View {
        if controller.streaming {
            StreamView(controller: controller)
        } else {
            switch controller.phase {
            case .restoring:
                SplashView()
            case .signedOut:
                WelcomeView(controller: controller, message: controller.status)
            case .failed(let message):
                WelcomeView(controller: controller, message: message)
            case .signingIn:
                SignInView(controller: controller)
            case .ready:
                HomeView(controller: controller)
            }
        }
    }
}

private struct SplashView: View {
    var body: some View {
        ZStack {
            Theme.glow
            VStack(spacing: 22) {
                PlaywakeLogo(size: 64)
                ProgressView().tint(Theme.mint)
            }
        }
    }
}

private struct WelcomeView: View {
    @ObservedObject var controller: SessionController
    let message: String

    var body: some View {
        ZStack {
            Theme.glow
            VStack(alignment: .leading, spacing: 0) {
                Spacer(minLength: 40)
                PlaywakeLogo(size: 76)
                Text("Playwake")
                    .font(.system(size: 40, weight: .bold))
                    .padding(.top, 22)
                Text("Stream your Xbox with the lowest latency we can get.")
                    .font(.system(size: 17))
                    .foregroundStyle(Color(hex: 0xC9CBCB))
                    .padding(.top, 10)
                Spacer()
                if !message.isEmpty {
                    Text(message)
                        .font(.footnote)
                        .foregroundStyle(Theme.danger)
                        .padding(.bottom, 14)
                }
                Button("Sign in with Microsoft") { controller.signIn() }
                    .buttonStyle(.playwake)
                Text("Use the Microsoft account your console is signed in to.")
                    .font(.footnote)
                    .foregroundStyle(Theme.muted)
                    .frame(maxWidth: .infinity)
                    .multilineTextAlignment(.center)
                    .padding(.top, 14)
                    .padding(.bottom, 24)
            }
            .frame(maxWidth: 420)
            .padding(.horizontal, 24)
        }
    }
}

private struct SignInView: View {
    @ObservedObject var controller: SessionController
    @Environment(\.openURL) private var openURL

    /// "microsoft.com/link" rather than the full URL with the code.
    static func display(_ url: URL) -> String {
        let host = (url.host ?? "microsoft.com").replacingOccurrences(of: "www.", with: "")
        return host + url.path
    }

    var body: some View {
        // Scrolls when the code, QR and buttons do not fit (iPhone landscape).
        ScrollView {
            content
                .frame(maxWidth: 420)
                .padding(.horizontal, 24)
                .frame(maxWidth: .infinity)
        }
        .scrollBounceBehavior(.basedOnSize)
    }

    private var content: some View {
        VStack(spacing: 0) {
            if let device = controller.device {
                Text("Sign in")
                    .font(.system(size: 34, weight: .bold))
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.top, 28)
                Text("Scan with your phone, or open the link and enter the code.")
                    .font(.system(size: 17))
                    .foregroundStyle(Color(hex: 0xC9CBCB))
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.top, 6)
                QRCodeView(text: device.url?.absoluteString ?? "https://www.microsoft.com/link", side: 200)
                    .padding(.top, 34)
                Text(device.code)
                    .font(.system(size: 34, weight: .semibold, design: .monospaced))
                    .tracking(4)
                    .textSelection(.enabled)
                    .padding(.top, 28)
                Spacer(minLength: 32)
                if let url = device.url {
                    Button("Open \(Self.display(url))") { openURL(url) }
                        .buttonStyle(.playwake)
                    if device.prefilled {
                        Text("The link already includes the code.")
                            .font(.footnote)
                            .foregroundStyle(Theme.muted)
                            .padding(.top, 10)
                    }
                }
                Button("Cancel") { controller.cancelSignIn() }
                    .buttonStyle(.playwakeSecondary)
                    .padding(.top, 14)
                    .padding(.bottom, 24)
            } else {
                ProgressView().tint(Theme.mint)
                    .padding(.top, 160)
                Text("Contacting Microsoft…")
                    .foregroundStyle(Theme.muted)
                    .padding(.top, 14)
                Button("Cancel") { controller.cancelSignIn() }
                    .buttonStyle(.playwakeSecondary)
                    .padding(.top, 120)
                    .padding(.bottom, 24)
            }
        }
    }
}
