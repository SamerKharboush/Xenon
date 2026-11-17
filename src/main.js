class PetRenderer {
    constructor() {
        this.canvas = document.getElementById('canvas');
        this.ctx = this.canvas.getContext('2d');
        this.state = 'Idle';
        this.resize();
        window.addEventListener('resize', () => this.resize());
        this.setupTauriListener();
        this.animate();
    }

    resize() {
        this.canvas.width = window.innerWidth;
        this.canvas.height = window.innerHeight;
    }

    setupTauriListener() {
        if (window.__TAURI__) {
            window.__TAURI__.event.listen('pet-state', (event) => {
                this.setState(event.payload);
            });
        }
    }

    setState(newState) {
        this.state = newState;
        this.draw();
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
        ctx.fillStyle = 'rgba(100, 200, 255, 0.6)';
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

        ctx.fillStyle = 'rgba(100, 200, 255, 0.6)';

        for (let i = 0; i < barCount; i++) {
            const height = Math.sin(time + i * 0.5) * maxHeight / 2 + maxHeight / 2;
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

        ctx.fillStyle = 'rgba(100, 200, 255, 0.6)';
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

        ctx.fillStyle = 'rgba(100, 200, 255, 0.6)';
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
}

document.addEventListener('DOMContentLoaded', () => {
    new PetRenderer();
});
