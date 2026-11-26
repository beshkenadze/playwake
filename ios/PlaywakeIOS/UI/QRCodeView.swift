import SwiftUI
import CoreImage
import CoreImage.CIFilterBuiltins

/// Renders a QR code for the Microsoft device-code sign-in flow, matching the
/// desktop client where the verification link is scannable.
struct QRCodeView: View {
    let text: String
    var side: CGFloat = 180

    var body: some View {
        if let image = Self.image(for: text) {
            Image(uiImage: image)
                .interpolation(.none)
                .resizable()
                .scaledToFit()
                .frame(width: side, height: side)
                .padding(side * 0.06)
                .background(Color.white)
                .clipShape(RoundedRectangle(cornerRadius: 10))
                .accessibilityLabel("Sign-in QR code")
        }
    }

    static func image(for text: String) -> UIImage? {
        guard let data = text.data(using: .utf8) else { return nil }
        let filter = CIFilter.qrCodeGenerator()
        filter.message = data
        filter.correctionLevel = "M"
        guard let output = filter.outputImage else { return nil }
        let scaled = output.transformed(by: CGAffineTransform(scaleX: 10, y: 10))
        guard let cgImage = CIContext().createCGImage(scaled, from: scaled.extent) else { return nil }
        return UIImage(cgImage: cgImage)
    }
}
