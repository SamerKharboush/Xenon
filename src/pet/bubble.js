const ACCENT = '#ff7aa1';
const INK = '#3a2b34';

export class Bubble {
    constructor() {
        this.text = null;
        this.tint = null;
        this.shownAt = 0;
        this.hideAfter = 0;
        this.popStart = -1;
    }

    show(text, tint = null, durationMs = 0) {
        if (!text) return;
        this.text = String(text).slice(0, 240);
        this.tint = tint;
        this.shownAt = performance.now();
        this.hideAfter = durationMs;
        this.popStart = this.shownAt;
    }

    hide() {
        this.text = null;
        this.tint = null;
    }

    update(now) {
        if (!this.text) return;
        if (this.hideAfter > 0 && now - this.shownAt > this.hideAfter) {
            this.hide();
        }
    }

    draw(ctx, cx, petTopY) {
        if (!this.text) return;
        const now = performance.now();
        const elapsed = now - this.popStart;
        const pop = Math.min(1, elapsed / 220);
        const scale = 0.92 + 0.08 * pop;
        const alpha = Math.min(1, elapsed / 140);

        const maxWidth = 250;
        const padding = 11;
        const font = '13px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif';
        ctx.font = font;

        const words = this.text.split(/\s+/);
        const lines = [];
        let line = '';
        for (const word of words) {
            const test = line ? line + ' ' + word : word;
            if (ctx.measureText(test).width > maxWidth - padding * 2 && line) {
                lines.push(line);
                line = word;
            } else {
                line = test;
            }
        }
        if (line) lines.push(line);

        const lineHeight = 17;
        const bubbleW = Math.min(
            maxWidth,
            Math.max(...lines.map((l) => ctx.measureText(l).width)) + padding * 2
        );
        const bubbleH = lines.length * lineHeight + padding * 2 - 4;

        const bubbleX = Math.round((320 - bubbleW) / 2);
        let bubbleY = petTopY - bubbleH - 14;
        if (bubbleY < 8) bubbleY = 8;

        const bg = this.tint || '#FFE3EA';

        ctx.save();
        ctx.globalAlpha = alpha;
        ctx.translate(160, bubbleY + bubbleH / 2);
        ctx.scale(scale, scale);
        ctx.translate(-160, -(bubbleY + bubbleH / 2));

        ctx.fillStyle = bg;
        roundRect(ctx, bubbleX, bubbleY, bubbleW, bubbleH, 14);
        ctx.fill();

        ctx.fillStyle = bg;
        ctx.beginPath();
        const tailX = 160;
        const tailY = bubbleY + bubbleH;
        ctx.moveTo(tailX - 8, tailY - 2);
        ctx.lineTo(tailX + 8, tailY - 2);
        ctx.lineTo(tailX, tailY + 9);
        ctx.closePath();
        ctx.fill();

        ctx.fillStyle = INK;
        ctx.textAlign = 'left';
        ctx.textBaseline = 'top';
        ctx.font = font;
        for (let i = 0; i < lines.length; i++) {
            const lx = bubbleX + padding;
            const ly = bubbleY + padding + i * lineHeight;
            ctx.fillText(lines[i], lx, ly);
        }
        ctx.restore();
    }
}

function roundRect(ctx, x, y, w, h, r) {
    const radius = Math.min(r, w / 2, h / 2);
    ctx.beginPath();
    ctx.moveTo(x + radius, y);
    ctx.lineTo(x + w - radius, y);
    ctx.arcTo(x + w, y, x + w, y + radius, radius);
    ctx.lineTo(x + w, y + h - radius);
    ctx.arcTo(x + w, y + h, x + w - radius, y + h, radius);
    ctx.lineTo(x + radius, y + h);
    ctx.arcTo(x, y + h, x, y + h - radius, radius);
    ctx.lineTo(x, y + radius);
    ctx.arcTo(x, y, x + radius, y, radius);
    ctx.closePath();
}

export const BUBBLE_COLORS = Object.freeze({
    speech: '#FFE3EA',
    notice: '#FFF1D6',
    accent: ACCENT,
    ink: INK,
});