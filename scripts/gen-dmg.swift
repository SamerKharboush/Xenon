#!/usr/bin/env swift
// Generate the Xenon DMG background: 660×400 @1x and @2x (1320×800) PNGs in
// src-tauri/icons/. Same blossom palette as the app icon (#fff5f8 → #ffdfeb),
// a soft arrow from the app slot to the Applications slot, and small
// "Drag Xenon to Applications" text. Tauri's DMG bundler picks these up via
// bundle.macOS.dmg.background in tauri.conf.json.
//
// Usage: swift scripts/gen-dmg.swift [outdir]   (default: src-tauri/icons)
//
// Layout (1×, y-down for drawing, window is 660×400):
//   app icon slot   ~ (165, 205)   Applications slot ~ (495, 205)

import AppKit
import Foundation

let outDir = CommandLine.arguments.count > 1
    ? CommandLine.arguments[1]
    : "src-tauri/icons"

let width = 660.0, height = 400.0
let appIconSize = 96.0

// Blossom gradient endpoints (pet-mochi palette, matches the app icon).
let gradTop = NSColor(srgbRed: 1.0, green: 0xf5 / 255.0, blue: 0xf8 / 255.0, alpha: 1.0)   // #fff5f8
let gradBottom = NSColor(srgbRed: 1.0, green: 0xdf / 255.0, blue: 0xeb / 255.0, alpha: 1.0) // #ffdfeb

func render(scale: CGFloat) -> Data? {
    let px = NSSize(width: width * scale, height: height * scale)
    let img = NSImage(size: px)
    img.lockFocus()
    guard let ctx = NSGraphicsContext.current?.cgContext else { img.unlockFocus(); return nil }
    ctx.scaleBy(x: scale, y: scale)

    // Background: vertical blossom gradient over the whole window.
    let rect = NSRect(x: 0, y: 0, width: width, height: height)
    NSGradient(starting: gradTop, ending: gradBottom)?.draw(in: rect, angle: 90)

    // A very light inner border so the window reads as a panel.
    NSColor.white.withAlphaComponent(0.6).setStroke()
    let border = NSBezierPath(roundedRect: rect.insetBy(dx: 0.5, dy: 0.5),
                              xRadius: 10, yRadius: 10)
    border.lineWidth = 1
    border.stroke()

    // Icon slot centers — must match bundle.macOS.dmg appPosition /
    // applicationFolderPosition in tauri.conf.json (y-down from top-left).
    let y = height - 170.0            // AppKit y-up: 170 from the top
    let appCX = 180.0, folderCX = 480.0

    // Placeholder rounded-square "app" slot.
    let appSlot = NSRect(x: appCX - appIconSize / 2, y: y - appIconSize / 2,
                         width: appIconSize, height: appIconSize)
    NSColor.white.withAlphaComponent(0.55).setFill()
    NSColor.white.withAlphaComponent(0.9).setStroke()
    let appPath = NSBezierPath(roundedRect: appSlot, xRadius: 20, yRadius: 20)
    appPath.lineWidth = 1.5
    appPath.fill()
    appPath.stroke()

    // Placeholder "Applications" folder slot.
    let fSlot = NSRect(x: folderCX - appIconSize / 2, y: y - appIconSize / 2,
                       width: appIconSize, height: appIconSize)
    NSColor.white.withAlphaComponent(0.35).setFill()
    NSColor.white.withAlphaComponent(0.7).setStroke()
    let fPath = NSBezierPath(roundedRect: fSlot, xRadius: 8, yRadius: 8)
    fPath.lineWidth = 1.5
    fPath.fill()
    fPath.stroke()

    // Arrow from app slot to folder slot.
    let fromX = appCX + appIconSize / 2 + 10
    let toX = folderCX - appIconSize / 2 - 14
    let ay = y
    let arrow = NSBezierPath()
    arrow.move(to: CGPoint(x: fromX, y: ay))
    arrow.line(to: CGPoint(x: toX - 12, y: ay))
    arrow.lineWidth = 3
    arrow.lineCapStyle = .round
    NSColor(calibratedRed: 0.45, green: 0.32, blue: 0.38, alpha: 1.0).setStroke()
    arrow.stroke()
    // Arrowhead.
    let head = NSBezierPath()
    head.move(to: CGPoint(x: toX, y: ay))
    head.line(to: CGPoint(x: toX - 14, y: ay + 8))
    head.line(to: CGPoint(x: toX - 14, y: ay - 8))
    head.close()
    NSColor(calibratedRed: 0.45, green: 0.32, blue: 0.38, alpha: 1.0).setFill()
    head.fill()

    // Small instruction text under the arrow.
    let text = "Drag Xenon to Applications"
    let para = NSMutableParagraphStyle()
    para.alignment = .center
    let attrs: [NSAttributedString.Key: Any] = [
        .font: NSFont.systemFont(ofSize: 13, weight: .medium),
        .foregroundColor: NSColor(calibratedRed: 0.35, green: 0.25, blue: 0.30, alpha: 1.0),
        .paragraphStyle: para,
    ]
    let str = NSAttributedString(string: text, attributes: attrs)
    let textRect = NSRect(x: 0, y: y - appIconSize / 2 - 42, width: width, height: 20)
    str.draw(in: textRect)

    img.unlockFocus()

    guard let tiff = img.tiffRepresentation,
          let rep = NSBitmapImageRep(data: tiff),
          let png = rep.representation(using: .png, properties: [:]) else { return nil }
    return png
}

try? FileManager.default.createDirectory(atPath: outDir, withIntermediateDirectories: true)

for (name, scale) in [("dmg-bg.png", 1.0), ("dmg-bg@2x.png", 2.0)] {
    guard let png = render(scale: scale) else {
        fputs("error: render failed for \(name)\n", stderr)
        exit(1)
    }
    let url = URL(fileURLWithPath: "\(outDir)/\(name)")
    try png.write(to: url)
    print("wrote \(url.path) (\(Int(width * scale))×\(Int(height * scale)))")
}