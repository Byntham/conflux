import AppKit

let arguments = CommandLine.arguments
guard arguments.count == 3,
      let svg = try? String(contentsOfFile: arguments[1], encoding: .utf8),
      let logo = Logo.path(svg: svg)
else {
    FileHandle.standardError.write("usage: make-icon <icon.svg> <out.iconset>\n".data(using: .utf8)!)
    exit(1)
}
let iconset = URL(fileURLWithPath: arguments[2])
try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)

func render(pixels: Int) -> Data {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
    )!
    NSGraphicsContext.saveGraphicsState()
    let graphics = NSGraphicsContext(bitmapImageRep: rep)!
    NSGraphicsContext.current = graphics
    let context = graphics.cgContext
    let scale = CGFloat(pixels) / Logo.box
    context.scaleBy(x: scale, y: scale)
    context.translateBy(x: 0, y: Logo.box)
    context.scaleBy(x: 1, y: -1)

    let body = CGRect(x: 100, y: 100, width: 824, height: 824)
    let shape = CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil)
    context.saveGState()
    context.setShadow(offset: CGSize(width: 0, height: 10), blur: 20, color: NSColor.black.withAlphaComponent(0.25).cgColor)
    context.addPath(shape)
    context.setFillColor(NSColor.white.cgColor)
    context.fillPath()
    context.restoreGState()

    context.saveGState()
    context.addPath(shape)
    context.clip()
    let gradient = CGGradient(
        colorsSpace: CGColorSpaceCreateDeviceRGB(),
        colors: [
            NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1).cgColor,
            NSColor(srgbRed: 0.89, green: 0.95, blue: 0.96, alpha: 1).cgColor,
        ] as CFArray,
        locations: [0, 1]
    )!
    context.drawLinearGradient(gradient, start: CGPoint(x: 0, y: 100), end: CGPoint(x: 0, y: 924), options: [])
    context.restoreGState()

    context.setFillColor(NSColor(srgbRed: 0.055, green: 0.373, blue: 0.447, alpha: 1).cgColor)
    Logo.draw(logo, in: body.insetBy(dx: 190, dy: 170), context: context)

    NSGraphicsContext.restoreGraphicsState()
    return rep.representation(using: .png, properties: [:])!
}

for points in [16, 32, 128, 256, 512] {
    for factor in [1, 2] {
        let name = factor == 1 ? "icon_\(points)x\(points).png" : "icon_\(points)x\(points)@2x.png"
        try render(pixels: points * factor).write(to: iconset.appendingPathComponent(name))
    }
}
