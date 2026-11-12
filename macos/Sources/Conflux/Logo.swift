import AppKit

enum Logo {
    static let box: CGFloat = 1024

    static func path(svg: String) -> CGPath? {
        guard let start = svg.range(of: " d=\""),
              let end = svg[start.upperBound...].firstIndex(of: "\"")
        else { return nil }
        return parse(String(svg[start.upperBound..<end]))
    }

    static func parse(_ d: String) -> CGPath {
        let path = CGMutablePath()
        var command: Character = "M"
        var numbers: [CGFloat] = []
        var token = ""

        func endNumber() {
            if let n = Double(token) { numbers.append(CGFloat(n)) }
            token = ""
        }
        func point(_ i: Int) -> CGPoint { CGPoint(x: numbers[i], y: numbers[i + 1]) }
        func endCommand() {
            endNumber()
            switch command {
            case "M", "L":
                for i in stride(from: 0, to: numbers.count - 1, by: 2) {
                    if command == "M" && i == 0 { path.move(to: point(i)) } else { path.addLine(to: point(i)) }
                }
            case "C":
                for i in stride(from: 0, to: numbers.count - 5, by: 6) {
                    path.addCurve(to: point(i + 4), control1: point(i), control2: point(i + 2))
                }
            case "Z", "z":
                path.closeSubpath()
            default:
                break
            }
            numbers = []
        }

        for c in d {
            if c.isLetter {
                endCommand()
                command = c
            } else if c == "-" {
                endNumber()
                token = "-"
            } else if c.isNumber || c == "." {
                token.append(c)
            } else {
                endNumber()
            }
        }
        endCommand()
        return path
    }

    static func draw(_ path: CGPath, in rect: CGRect, context: CGContext) {
        let bounds = path.boundingBoxOfPath
        let scale = min(rect.width / bounds.width, rect.height / bounds.height)
        context.saveGState()
        context.translateBy(
            x: rect.midX - bounds.midX * scale,
            y: rect.midY - bounds.midY * scale
        )
        context.scaleBy(x: scale, y: scale)
        context.addPath(path)
        context.fillPath()
        context.restoreGState()
    }
}
