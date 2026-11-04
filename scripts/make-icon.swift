// Renders the app icon into an .iconset and builds AppIcon.icns with iconutil.
//   swift scripts/make-icon.swift crates/pi-desktop/assets/AppIcon.icns
// A blue rounded square (the design's accent, #123BB5 → #3B6CF0) with a
// white serif pi glyph "π". Re-run after changing the drawing; the .icns
// is committed so bundling needs no Swift.
import AppKit

let output = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "AppIcon.icns"
let iconset = URL(fileURLWithPath: NSTemporaryDirectory()).appendingPathComponent("AppIcon.iconset")
try? FileManager.default.removeItem(at: iconset)
try! FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)

func color(_ hex: UInt32) -> CGColor {
	CGColor(red: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255, blue: CGFloat(hex & 0xff) / 255, alpha: 1)
}

func render(_ px: Int) -> Data {
	let s = CGFloat(px)
	let ctx = CGContext(data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0,
		space: CGColorSpace(name: CGColorSpace.sRGB)!, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
	// macOS icon grid: 824/1024 body, centred, corner radius ~185/1024.
	let inset = s * 100 / 1024
	let body = CGRect(x: inset, y: inset, width: s - 2 * inset, height: s - 2 * inset)
	let shape = CGPath(roundedRect: body, cornerWidth: s * 185 / 1024, cornerHeight: s * 185 / 1024, transform: nil)
	ctx.saveGState()
	ctx.setShadow(offset: CGSize(width: 0, height: -s * 10 / 1024), blur: s * 28 / 1024, color: CGColor(gray: 0, alpha: 0.3))
	ctx.addPath(shape)
	ctx.setFillColor(color(0x123BB5))
	ctx.fillPath()
	ctx.restoreGState()
	ctx.saveGState()
	ctx.addPath(shape)
	ctx.clip()
	let gradient = CGGradient(colorsSpace: nil, colors: [color(0x3B6CF0), color(0x123BB5)] as CFArray, locations: [0, 1])!
	ctx.drawLinearGradient(gradient, start: CGPoint(x: 0, y: body.maxY), end: CGPoint(x: 0, y: body.minY), options: [])
	ctx.restoreGState()
	// "π" drawn as three strokes: top bar with small serifs, two legs with
	// rounded feet — the right leg curves out at the bottom like a serif π.
	ctx.setStrokeColor(CGColor(gray: 1, alpha: 1))
	ctx.setLineCap(.round)
	ctx.setLineJoin(.round)
	let u = s / 1024
	// Top bar (with short serif ticks at both ends).
	ctx.setLineWidth(s * 72 / 1024)
	ctx.move(to: CGPoint(x: 268 * u, y: 640 * u))
	ctx.addLine(to: CGPoint(x: 756 * u, y: 640 * u))
	ctx.strokePath()
	ctx.setLineWidth(s * 52 / 1024)
	ctx.move(to: CGPoint(x: 268 * u, y: 696 * u))
	ctx.addLine(to: CGPoint(x: 268 * u, y: 640 * u))
	ctx.move(to: CGPoint(x: 756 * u, y: 696 * u))
	ctx.addLine(to: CGPoint(x: 756 * u, y: 640 * u))
	ctx.strokePath()
	// Left leg: straight, slight italic lean, ending in a rounded foot.
	ctx.setLineWidth(s * 68 / 1024)
	let left = CGMutablePath()
	left.move(to: CGPoint(x: 430 * u, y: 640 * u))
	left.addLine(to: CGPoint(x: 400 * u, y: 372 * u))
	left.addQuadCurve(to: CGPoint(x: 348 * u, y: 352 * u), control: CGPoint(x: 388 * u, y: 352 * u))
	ctx.addPath(left)
	ctx.strokePath()
	// Right leg: straight down, then hooks right at the bottom.
	let right = CGMutablePath()
	right.move(to: CGPoint(x: 612 * u, y: 640 * u))
	right.addLine(to: CGPoint(x: 588 * u, y: 396 * u))
	right.addQuadCurve(to: CGPoint(x: 668 * u, y: 352 * u), control: CGPoint(x: 584 * u, y: 352 * u))
	ctx.addPath(right)
	ctx.strokePath()
	let rep = NSBitmapImageRep(cgImage: ctx.makeImage()!)
	return rep.representation(using: .png, properties: [:])!
}

for points in [16, 32, 128, 256, 512] {
	for scale in [1, 2] {
		let name = scale == 1 ? "icon_\(points)x\(points).png" : "icon_\(points)x\(points)@2x.png"
		try! render(points * scale).write(to: iconset.appendingPathComponent(name))
	}
}
let task = Process()
task.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
task.arguments = ["-c", "icns", iconset.path, "-o", output]
try! task.run()
task.waitUntilExit()
exit(task.terminationStatus)
