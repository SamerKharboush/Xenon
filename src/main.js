const STATE_COLORS = {
    Idle: 'rgba(100, 200, 255, 0.6)',
    Listening: 'rgba(100, 200, 255, 0.6)',
    Thinking: 'rgba(100, 200, 255, 0.6)',
    Speaking: 'rgba(100, 200, 255, 0.6)',
    Agentic: 'rgba(100, 200, 255, 0.6)',
};

class PetRenderer {
    constructor() {
        this.canvas = document.getElementById('canvas');
        this.ctx = this.canvas.getContext('2d');
        this.state = 'Idle';
        this.resize();
        window.addEventListener('resize', () => this.resize());
        this.setupTauriListener();
        this.draw();
        this.animate();
        this.reportReady();
    }

    resize() {
        this.canvas.width = window.innerWidth;
        this.canvas.height = window.innerHeight;
    }

    setupTauriListener() {
        const tauri = window.__TAURI__;
        if (!tauri) return;
        const { listen, invoke } = tauri.event;
        listen('pet-state', (event) => this.setState(event.payload));
        this.invoke = invoke;
    }

    // Idle is click-through; any active state needs real input (ADR: see ui.rs).
    setState(newState) {
        this.state = newState;
        if (this.invoke) {
            this.invoke('set_click_through', { ignore: newState === 'Idle' }).catch(() => {});
        }
    }

    draw() {
        const ctx = this.ctx;
        const centerX = this.canvas.width / 2;
        const centerY = this.canvas.height / 2;

        ctx.clearRect(0, 0, this.canvas.width, this.canvas.height);

        switch (this.state) {
            case 'Listening':
                this.drawWaveform(centerX, centerY);
                break;
            case 'Thinking':
                this.drawPulse(centerX, centerY);
                break;
            case 'Speaking':
                this.drawLipSync(centerX, centerY);
                break;
            case 'Agentic':
                this.drawTrackingEyes(centerX, centerY);
                break;
            case 'Idle':
            default:
                this.drawIdle(centerX, centerY);
                break;
        }
    }

    drawIdle(x, y) {
        const ctx = this.ctx;
        ctx.fillStyle = STATE_COLORS.Idle;
        ctx.beginPath();
        ctx.arc(x, y, 30, 0, Math.PI * 2);
        ctx.fill();

        ctx.fillStyle = 'rgba(0, 0, 0, 0.8)';
        ctx.beginPath();
        ctx.arc(x - 10, y - 5, 4, 0, Math.PI * 2);
        ctx.arc(x + 10, y - 5, 4, 0, Math.PI * 2);
        ctx.fill();
    }

    drawWaveform(x, y) {
        const ctx = this.ctx;
        const time = Date.now() / 100;
        const barWidth = 4;
        const barSpacing = 2;
        const barCount = 20;
        const maxHeight = 40;

        ctx.fillStyle = STATE_COLORS.Listening;

        for (let i = 0; i < barCount; i++) {
            const height = (Math.sin(time + i * 0.5) * maxHeight) / 2 + maxHeight / 2;
            const barX = x - (barCount * (barWidth + barSpacing)) / 2 + i * (barWidth + barSpacing);
            ctx.fillRect(barX, y - height / 2, barWidth, height);
        }
    }

    drawPulse(x, y) {
        const ctx = this.ctx;
        const time = Date.now() / 500;
        const scale = 1 + Math.sin(time) * 0.3;

        ctx.fillStyle = `rgba(100, 200, 255, ${0.4 + Math.sin(time) * 0.2})`;
        ctx.beginPath();
        ctx.arc(x, y, 30 * scale, 0, Math.PI * 2);
        ctx.fill();
    }

    drawLipSync(x, y) {
        const ctx = this.ctx;
        const time = Date.now() / 100;
        const openness = (Math.sin(time * 3) + 1) / 2;

        ctx.fillStyle = STATE_COLORS.Speaking;
        ctx.beginPath();
        ctx.arc(x, y, 30, 0, Math.PI * 2);
        ctx.fill();

        ctx.fillStyle = 'rgba(0, 0, 0, 0.8)';
        ctx.beginPath();
        ctx.ellipse(x, y + 5, 8 + openness * 6, 4 + openness * 8, 0, 0, Math.PI * 2);
        ctx.fill();
    }

    drawTrackingEyes(x, y) {
        const ctx = this.ctx;
        const time = Date.now() / 200;
        const lookX = Math.sin(time) * 8;
        const lookY = Math.cos(time * 0.7) * 6;

        ctx.fillStyle = STATE_COLORS.Agentic;
        ctx.beginPath();
        ctx.arc(x, y, 30, 0, Math.PI * 2);
        ctx.fill();

        ctx.fillStyle = 'rgba(0, 0, 0, 0.8)';
        ctx.beginPath();
        ctx.arc(x - 10 + lookX, y - 5 + lookY, 4, 0, Math.PI * 2);
        ctx.arc(x + 10 + lookX, y - 5 + lookY, 4, 0, Math.PI * 2);
        ctx.fill();
    }

    animate() {
        this.draw();
        requestAnimationFrame(() => this.animate());
    }

    // Startup handshake: tells the Rust side the page loaded and that the
    // canvas actually has pixels, so rendering can be verified without a
    // screenshot (screen capture needs Screen Recording permission).
    reportReady() {
        const invoke = window.__TAURI__ && window.__TAURI__.event.invoke;
        if (!invoke) {
            console.error('[xenon] __TAURI__ global missing (withGlobalTauri off?)');
            return;
        }
        const { width, height } = this.canvas;
        const data = this.ctx.getImageData(0, 0, width, height).data;
        let opaque = 0;
        for (let i = 3; i < data.length; i += 4) {
            if (data[i] > 0) opaque++;
        }
        invoke('frontend_ready', { width, height, opaquePixels: opaque }).catch((e) =>
            console.error('[xenon] frontend_ready failed', e)
        );
    }
}

document.addEventListener('DOMContentLoaded', () => {
    new PetRenderer();
});
