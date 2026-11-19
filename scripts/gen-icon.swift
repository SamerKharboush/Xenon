#!/usr/bin/env swift
// Generate the Xenon app icon: the pet-mochi idle sprite centered on a rounded-
// square macOS-style background filled with the pet-mochi blossom gradient
// (#fff5f8 → #ffdfeb) and a soft drop shadow. ~10% padding per Apple's icon
// grid. Output: src-tauri/icons/source.png (1024×1024).
//
// Usage: swift scripts/gen-icon.swift [output.png]
// Then run `cargo tauri icon src-tauri/icons/source.png` to produce the rest.

import AppKit
import Foundation

// --- Config ------------------------------------------------------------------
let outPath = CommandLine.arguments.count > 1
    ? CommandLine.arguments[1]
    : "src-tauri/icons/source.png"

// The sprite, relative to the repo root. Override with XENON_SPRITE=...
let spritePath = ProcessInfo.processInfo.environment["XENON_SPRITE"]
    ?? "src/sprites/mochi-idle.png"

let canvas = 1024.0
let radius = 224.0            // macOS rounded-square radius (~22% of side)
let pad = 102.0              // ~10% Apple icon grid padding
let iconSide = canvas - 2 * pad

// Blossom gradient endpoints (pet-mochi palette).
let gradTop = NSColor(srgbRed: 1.0, green: 0xf5 / 255.0, blue: 0xf8 / 255.0, alpha: 1.0)  // #fff5f8
let gradBottom = NSColor(srgbRed: 1.0, green: 0xdf / 255.0, blue: 0xeb / 255.0, alpha: 1.0) // #ffdfeb

guard let sprite = NSImage(contentsOfFile: spritePath) else {
    fputs("error: could not load sprite at \(spritePath)\n", stderr)
    exit(1)
}

// --- Render ------------------------------------------------------------------
let size = NSSize(width: canvas, height: canvas)
let img = NSImage(size: size)
img.lockFocus()

// Background: subtle drop shadow behind the rounded square.
let bgRect = NSRect(x: pad, y: pad, width: iconSide, height: iconSide)
let shadow = NSShadow()
shadow.shadowColor = NSColor.black.withAlphaComponent(0.18)
shadow.shadowBlurRadius = 28
shadow.shadowOffset = NSSize(width: 0, height: -10)

let path = NSBezierPath(roundedRect: bgRect, xRadius: radius, yRadius: radius)
NSColor.clear.setFill()
shadow.set()
path.fill()

// Rounded-square gradient fill — clip to the rounded path first so the
// gradient (which fills the whole rect) doesn't square off the corners.
NSGraphicsContext.saveGraphicsState()
path.setClip()
if let gradient = NSGradient(starting: gradTop, ending: gradBottom) {
    gradient.draw(in: bgRect, angle: 90)
}
NSGraphicsContext.restoreGraphicsState()

// Sprite: fit inside the rounded square with a little breathing room.
let spriteRect = bgRect.insetBy(dx: iconSide * 0.06, dy: iconSide * 0.06)
let aspect = sprite.size.width / sprite.size.height
var drawRect = spriteRect
if aspect > 1 {
    let h = spriteRect.width / aspect
    drawRect = NSRect(x: spriteRect.minX, y: spriteRect.minY + (spriteRect.height - h) / 2,
                      width: spriteRect.width, height: h)
} else {
    let w = spriteRect.height * aspect
    drawRect = NSRect(x: spriteRect.minX + (spriteRect.width - w) / 2, y: spriteRect.minY,
                      width: w, height: spriteRect.height)
}
sprite.draw(in: drawRect,
            from: NSRect(x: 0, y: 0, width: sprite.size.width, height: sprite.size.height),
            operation: .sourceOver, fraction: 1.0,
            respectFlipped: false, hints: nil)

img.unlockFocus()

// --- Write PNG ---------------------------------------------------------------
guard let tiff = img.tiffRepresentation,
      let rep = NSBitmapImageRep(data: tiff) else {
    fputs("error: could not encode image\n", stderr)
    exit(1)
}
guard let png = rep.representation(using: .png, properties: [:]) else {
    fputs("error: could not encode PNG\n", stderr)
    exit(1)
}
let url = URL(fileURLWithPath: outPath)
try? FileManager.default.createDirectory(at: url.deletingLastPathComponent(),
                                         withIntermediateDirectories: true)
do {
    try png.write(to: url)
    print("wrote \(outPath) (\(Int(canvas))×\(Int(canvas)))")
} catch {
    fputs("error: \(error)\n", stderr)
    exit(1)
}