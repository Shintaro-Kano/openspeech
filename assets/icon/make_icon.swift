import CoreGraphics
import ImageIO
import Foundation
import UniformTypeIdentifiers

let cs = CGColorSpace(name: CGColorSpace.sRGB)!
func col(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(colorSpace: cs, components: [CGFloat((hex >> 16) & 255) / 255, CGFloat((hex >> 8) & 255) / 255, CGFloat(hex & 255) / 255, a])!
}

enum Shape { case squircle, circle }

func render(size: Int, shape: Shape) -> CGImage {
    let ctx = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0, space: cs,
                        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.setAllowsAntialiasing(true); ctx.setShouldAntialias(true); ctx.interpolationQuality = .high
    let s = CGFloat(size) / 1024
    ctx.scaleBy(x: s, y: s)
    let small = size <= 32
    let tiny = size <= 16

    // Body shape
    let body: CGPath
    if shape == .squircle {
        body = CGPath(roundedRect: CGRect(x: 100, y: 100, width: 824, height: 824), cornerWidth: 185, cornerHeight: 185, transform: nil)
    } else {
        body = CGPath(ellipseIn: CGRect(x: 32, y: 32, width: 960, height: 960), transform: nil)
    }
    ctx.saveGState()
    ctx.addPath(body); ctx.clip()
    let bg = CGGradient(colorsSpace: cs, colors: [col(0x5FA9F0), col(0xBFE3FF)] as CFArray, locations: [0, 1])!
    ctx.drawLinearGradient(bg, start: CGPoint(x: 0, y: 100), end: CGPoint(x: 0, y: 924), options: [])
    let hl = CGGradient(colorsSpace: cs, colors: [col(0xFFFFFF, 0.55), col(0xFFFFFF, 0)] as CFArray, locations: [0, 1])!
    ctx.drawRadialGradient(hl, startCenter: CGPoint(x: 340, y: 800), startRadius: 0, endCenter: CGPoint(x: 340, y: 800), endRadius: 560, options: [])
    ctx.restoreGState()

    // Mic group (scaled about canvas centre for the circle variant)
    ctx.saveGState()
    if shape == .circle {
        ctx.translateBy(x: 512, y: 512); ctx.scaleBy(x: 1.12, y: 1.12); ctx.translateBy(x: -512, y: -512)
    }
    let cx: CGFloat = small ? 470 : 452
    let lw: CGFloat = tiny ? 62 : (small ? 50 : 38)
    ctx.setShadow(offset: CGSize(width: 0, height: -16), blur: small ? 14 : 26, color: col(0x1F5FA8, 0.38))
    ctx.beginTransparencyLayer(auxiliaryInfo: nil)
    let white = col(0xFFFFFF)
    ctx.setFillColor(white); ctx.setStrokeColor(white)
    ctx.setLineCap(.round); ctx.setLineJoin(.round)

    // Capsule
    let capW: CGFloat = 240, capH: CGFloat = 350, capY: CGFloat = 440
    ctx.addPath(CGPath(roundedRect: CGRect(x: cx - capW / 2, y: capY, width: capW, height: capH), cornerWidth: capW / 2, cornerHeight: capW / 2, transform: nil))
    ctx.fillPath()
    // Cradle
    ctx.setLineWidth(lw)
    let cr: CGFloat = 172, cy: CGFloat = 575
    ctx.addArc(center: CGPoint(x: cx, y: cy), radius: cr, startAngle: .pi, endAngle: 2 * .pi, clockwise: false)
    ctx.strokePath()
    // Stem + base
    ctx.move(to: CGPoint(x: cx, y: cy - cr)); ctx.addLine(to: CGPoint(x: cx, y: 290)); ctx.strokePath()
    ctx.move(to: CGPoint(x: cx - 100, y: 280)); ctx.addLine(to: CGPoint(x: cx + 100, y: 280)); ctx.strokePath()
    // Waves
    let waveCount = tiny ? 1 : (small ? 2 : 3)
    let radii: [CGFloat] = [228, 292, 356]
    let wc = CGPoint(x: cx, y: 615)
    for i in 0..<waveCount {
        ctx.setLineWidth(tiny ? 54 : (small ? 44 : 34))
        ctx.setStrokeColor(col(0xFFFFFF, 1 - 0.14 * CGFloat(i)))
        let a: CGFloat = 36 * .pi / 180
        ctx.addArc(center: wc, radius: radii[i], startAngle: -a, endAngle: a, clockwise: false)
        ctx.strokePath()
    }
    ctx.endTransparencyLayer()
    ctx.restoreGState()

    // Face (not at small sizes)
    if !small {
        ctx.saveGState()
        if shape == .circle {
            ctx.translateBy(x: 512, y: 512); ctx.scaleBy(x: 1.12, y: 1.12); ctx.translateBy(x: -512, y: -512)
        }
        let blue = col(0x3C84D8)
        ctx.setFillColor(col(0xFF9DB8, 0.55))
        for dx: CGFloat in [-84, 84] {
            ctx.fillEllipse(in: CGRect(x: cx + dx - 26, y: 598 - 17, width: 52, height: 34))
        }
        ctx.setFillColor(blue)
        for dx: CGFloat in [-50, 50] {
            ctx.fillEllipse(in: CGRect(x: cx + dx - 19, y: 655 - 23, width: 38, height: 46))
        }
        ctx.setStrokeColor(blue); ctx.setLineWidth(15); ctx.setLineCap(.round)
        let a: CGFloat = 52 * .pi / 180
        ctx.addArc(center: CGPoint(x: cx, y: 622), radius: 42, startAngle: -.pi / 2 - a, endAngle: -.pi / 2 + a, clockwise: false)
        ctx.strokePath()
        ctx.restoreGState()
    }
    return ctx.makeImage()!
}

func writePNG(_ img: CGImage, to url: URL) {
    let d = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil)!
    CGImageDestinationAddImage(d, img, nil)
    if !CGImageDestinationFinalize(d) { fatalError("write failed \(url.path)") }
}

@main struct Main {
    static func main() throws {
        let out = URL(fileURLWithPath: CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : ".", isDirectory: true)
        let fm = FileManager.default
        try fm.createDirectory(at: out, withIntermediateDirectories: true)
        var cache: [Int: CGImage] = [:]
        func icon(_ n: Int) -> CGImage { if let c = cache[n] { return c }; let i = render(size: n, shape: .squircle); cache[n] = i; return i }
        for n in [16, 32, 64, 128, 256, 512, 1024] { writePNG(icon(n), to: out.appendingPathComponent("icon_\(n).png")) }
        writePNG(render(size: 64, shape: .circle), to: out.appendingPathComponent("window_64.png"))
        let set = out.appendingPathComponent("OpenSpeech.iconset", isDirectory: true)
        try? fm.removeItem(at: set)
        try fm.createDirectory(at: set, withIntermediateDirectories: true)
        for b in [16, 32, 128, 256, 512] {
            writePNG(icon(b), to: set.appendingPathComponent("icon_\(b)x\(b).png"))
            writePNG(icon(b * 2), to: set.appendingPathComponent("icon_\(b)x\(b)@2x.png"))
        }
    }
}
