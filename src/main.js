// Xenon Pet Renderer - Mochi-level fluid animations + Jarvis-like interactivity
const STATES = {
    IDLE: 'Idle',
    LISTENING: 'Listening',
    THINKING: 'Thinking',
    SPEAKING: 'Speaking',
    AGENTIC: 'Agentic',
};

const COLORS = {
    body: 'rgba(100, 200, 255, 0.85)',
    bodyGlow: 'rgba(100, 200, 255, 0.3)',
    eyes: 'rgba(0, 0, 0, 0.9)',
    eyesGlow: 'rgba(100, 200, 255, 0.6)',
    mouth: 'rgba(0, 0, 0, 0.8)',
    accent: 'rgba(255, 255, 255, 0.9)',
};

class PetRenderer {
    constructor() {
        this.canvas = document.getElementById('canvas');
        this.ctx = this.canvas.getContext('2d');
        this.state = STATES.IDLE;
        this.prevState = STATES.IDLE;
        this.stateStart = performance.now();
        
        // Animation state
        this.breathPhase = 0;
        this.blinkTimer = 0;
        this.nextBlink = 1200 + Math.random() * 2200;
        this.isBlinking = false;
        this.lookTarget = { x: 0, y: 0 };
        this.lookCurrent = { x: 0, y: 0 };
        this.mousePos = { x: 0, y: 0 };
        this.lastInteraction = performance.now();
        
        // Particles for effects
        this.particles = [];
        
        // Idle behaviors
        this.idleBehaviorTimer = 0;
        this.nextIdleBehavior = 2500 + Math.random() * 3000;
        this.currentBehavior = null;
        
        this.resize();
        window.addEventListener('resize', () => this.resize());
        this.setupTauriListener();
        this.setupMouseTracking();
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
        const { listen } = tauri.event;
        const { invoke } = tauri.core;
        this.invoke = invoke;

        listen('pet-state', (event) => {
            this.setState(event.payload);
        });

        listen('pet-speech', (event) => {
            // Sentence-by-sentence TTS hook. Real lip-sync is a later task.
            // console.log('[xenon] speak:', event.payload);
        });

        // Real mouse input on the webview.
        this.canvas.addEventListener('click', () => this.onPetClick());
        this.canvas.addEventListener('dblclick', () => this.onPetDoubleClick());
        this.canvas.addEventListener('contextmenu', (e) => {
            e.preventDefault();
            this.onPetDoubleClick();
        });
        this.canvas.addEventListener('mouseenter', () => this.onPetHover(true));
        this.canvas.addEventListener('mouseleave', () => this.onPetHover(false));
    }

    setupMouseTracking() {
        document.addEventListener('mousemove', (e) => {
            this.mousePos.x = e.clientX;
            this.mousePos.y = e.clientY;
            this.lastInteraction = performance.now();
            
            // Update look target based on mouse position
            const centerX = this.canvas.width / 2;
            const centerY = this.canvas.height / 2;
            const dx = e.clientX - centerX;
            const dy = e.clientY - centerY;
            const dist = Math.sqrt(dx * dx + dy * dy);
            const maxDist = 100;
            
            this.lookTarget.x = Math.max(-8, Math.min(8, (dx / maxDist) * 8));
            this.lookTarget.y = Math.max(-6, Math.min(6, (dy / maxDist) * 6));
        });
        
        // Smooth look interpolation
        const smoothLook = () => {
            this.lookCurrent.x += (this.lookTarget.x - this.lookCurrent.x) * 0.1;
            this.lookCurrent.y += (this.lookTarget.y - this.lookCurrent.y) * 0.1;
            requestAnimationFrame(smoothLook);
        };
        smoothLook();
    }

    setState(newState) {
        if (this.state === newState) return;

        this.prevState = this.state;
        this.state = newState;
        this.stateStart = performance.now();
        this.currentBehavior = null;

        // Emit particles on state change
        this.emitParticles(newState);
        // Click-through is owned by Rust (cursor hit-test); do not fight it here.
    }

    emitParticles(state) {
        const colors = {
            [STATES.LISTENING]: '100, 200, 255',
            [STATES.THINKING]: '255, 200, 100',
            [STATES.SPEAKING]: '100, 255, 150',
            [STATES.AGENTIC]: '255, 150, 255',
        };
        
        const color = colors[state] || '100, 200, 255';
        const cx = this.canvas.width / 2;
        const cy = this.canvas.height / 2;
        
        for (let i = 0; i < 12; i++) {
            const angle = (i / 12) * Math.PI * 2;
            const speed = 1 + Math.random() * 2;
            this.particles.push({
                x: cx,
                y: cy,
                vx: Math.cos(angle) * speed,
                vy: Math.sin(angle) * speed,
                life: 1.0,
                decay: 0.02 + Math.random() * 0.02,
                size: 2 + Math.random() * 3,
                color: color,
            });
        }
    }

    updateParticles() {
        this.particles = this.particles.filter(p => p.life > 0);
        for (const p of this.particles) {
            p.x += p.vx;
            p.y += p.vy;
            p.life -= p.steadyDecay || p.decay;
            p.vy += 0.05; // gravity
        }
    }

    drawParticles() {
        const ctx = this.ctx;
        for (const p of this.particles) {
            const alpha = p.life;
            ctx.fillStyle = `rgba(${p.color}, ${alpha})`;
            ctx.beginPath();
            ctx.arc(p.x, p.y, p.size * p.life, 0, Math.PI * 2);
            ctx.fill();
        }
    }

    onPetClick() {
        this.lastInteraction = performance.now();
        // Ask Rust to have the pet greet out loud (Jarvis-style).
        // Rust drives the state machine (Thinking → Speaking → Idle).
        if (this.invoke) {
            this.invoke('pet_tap').catch(() => {});
        }
    }

    onPetDoubleClick() {
        this.lastInteraction = performance.now();
        if (this.invoke) {
            this.invoke('open_settings').catch(() => {});
        }
    }

    onPetHover(isHovering) {
        this.lastInteraction = performance.now();
        if (isHovering && this.state === STATES.IDLE && !this.currentBehavior) {
            // Look at cursor when hovered
            this.lookTarget.x = 0;
            this.lookTarget.y = -3;
        }
    }

    updateIdleBehavior(now) {
        if (this.state !== STATES.IDLE) return;

        if (!this.currentBehavior && now - this.stateStart > this.nextIdleBehavior) {
            // Pick random behavior
            const behaviors = ['look_around', 'stretch', 'bounce', 'tilt'];
            this.currentBehavior = {
                type: behaviors[Math.floor(Math.random() * behaviors.length)],
                start: now,
                duration: 2000 + Math.random() * 2000,
            };
        }

        if (this.currentBehavior) {
            const elapsed = now - this.currentBehavior.start;
            if (elapsed > this.currentBehavior.duration) {
                this.currentBehavior = null;
                this.nextIdleBehavior = 2000 + Math.random() * 3500;
            }
        }
    }

    draw() {
        const ctx = this.ctx;
        const now = performance.now();
        const centerX = this.canvas.width / 2;
        const centerY = this.canvas.height / 2;

        ctx.clearRect(0, 0, this.canvas.width, this.canvas.height);

        // Update breathing
        this.breathPhase = Math.sin(now / 1000) * 0.05;

        // Apply idle behavior transformations
        let offsetX = 0, offsetY = 0, scaleX = 1, scaleY = 1, rotation = 0;

        // Gentle floating bob so the pet is visibly alive while idle.
        const bob = Math.sin(now / 900) * 3.5;
        offsetY += bob;

        // Update blinking
        this.updateBlink(now);
        
        // Update idle behaviors
        this.updateIdleBehavior(now);
        
        // Update particles
        this.updateParticles();
        this.drawParticles();

        if (this.currentBehavior) {
            const t = (now - this.currentBehavior.start) / this.currentBehavior.duration;
            const ease = t < 0.5 ? 2 * t * t : -1 + (4 - 2 * t) * t;
            
            switch (this.currentBehavior.type) {
                case 'look_around': {
                    const angle = ease * Math.PI * 2;
                    this.lookTarget.x = Math.cos(angle) * 6;
                    this.lookTarget.y = Math.sin(angle) * 4;
                    break;
                }
                case 'stretch': {
                    const stretch = Math.sin(ease * Math.PI);
                    scaleX = 1 + stretch * 0.2;
                    scaleY = 1 + stretch * 0.2;
                    offsetY = -stretch * 10;
                    break;
                }
                case 'bounce': {
                    const bounce = Math.abs(Math.sin(ease * Math.PI * 3)) * 15;
                    offsetY = -bounce;
                    scaleY = 1 + (bounce / 100);
                    scaleX = 1 - (bounce / 200);
                    break;
                }
                case 'tilt': {
                    rotation = Math.sin(ease * Math.PI * 2) * 0.2;
                    break;
                }
            }
        }

        ctx.save();
        ctx.translate(centerX + offsetX, centerY + offsetY);
        ctx.rotate(rotation);
        ctx.scale(scaleX * (1 + this.breathPhase), scaleY * (1 + this.breathPhase));

        // Draw based on state
        switch (this.state) {
            case STATES.LISTENING:
                this.drawListening(ctx, now);
                break;
            case STATES.THINKING:
                this.drawThinking(ctx, now);
                break;
            case STATES.SPEAKING:
                this.drawSpeaking(ctx, now);
                break;
            case STATES.AGENTIC:
                this.drawAgentic(ctx, now);
                break;
            case STATES.IDLE:
            default:
                this.drawIdle(ctx, now);
                break;
        }

        ctx.restore();
    }

    updateBlink(now) {
        if (!this.isBlinking && now > this.nextBlink) {
            this.isBlinking = true;
            this.blinkTimer = now;
        }
        
        if (this.isBlinking) {
            const elapsed = now - this.blinkTimer;
            if (elapsed > 150) {
                this.isBlinking = false;
                this.nextBlink = now + 1200 + Math.random() * 2200;
            }
        }
    }

    getBlinkScale() {
        if (!this.isBlinking) return 1;
        const elapsed = performance.now() - this.blinkTimer;
        if (elapsed < 50) return 1 - (elapsed / 50);
        if (elapsed < 100) return (elapsed - 50) / 50;
        return 1;
    }

    drawBody(ctx) {
        // Glow
        ctx.fillStyle = COLORS.bodyGlow;
        ctx.beginPath();
        ctx.arc(0, 0, 35, 0, Math.PI * 2);
        ctx.fill();
        
        // Main body
        ctx.fillStyle = COLORS.body;
        ctx.beginPath();
        ctx.arc(0, 0, 30, 0, Math.PI * 2);
        ctx.fill();
        
        // Highlight
        const gradient = ctx.createRadialGradient(-5, -8, 0, 0, 0, 30);
        gradient.addColorStop(0, 'rgba(255, 255, 255, 0.3)');
        gradient.addColorStop(1, 'rgba(255, 255, 255, 0)');
        ctx.fillStyle = gradient;
        ctx.beginPath();
        ctx.arc(0, 0, 30, 0, Math.PI * 2);
        ctx.fill();
    }

    drawEyes(ctx, lookX = 0, lookY = 0, scale = 1) {
        ctx.save();
        ctx.scale(1, scale);
        ctx.fillStyle = COLORS.eyes;
        ctx.beginPath();
        ctx.arc(-10 + lookX, -5 + lookY, 4, 0, Math.PI * 2);
        ctx.arc(10 + lookX, -5 + lookY, 4, 0, Math.PI * 2);
        ctx.fill();
        
        // Eye shine
        ctx.fillStyle = 'rgba(255, 255, 255, 0.8)';
        ctx.beginPath();
        ctx.arc(-11 + lookX, -6 + lookY, 1.5, 0, Math.PI * 2);
        ctx.arc(9 + lookX, -6 + lookY, 1.5, 0, Math.PI * 2);
        ctx.fill();
        ctx.restore();
    }

    drawMouth(ctx, openness = 0, x = 0, y = 8) {
        if (openness < 0.1) {
            // Smile
            ctx.strokeStyle = COLORS.mouth;
            ctx.lineWidth = 1.5;
            ctx.beginPath();
            ctx.arc(x, y - 2, 4, 0.1 * Math.PI, 0.9 * Math.PI);
            ctx.stroke();
        } else {
            // Open mouth
            ctx.fillStyle = COLORS.mouth;
            ctx.beginPath();
            ctx.ellipse(x, y, 5, 2 + openness * 6, 0, 0, Math.PI * 2);
            ctx.fill();
        }
    }

    drawIdle(ctx, now) {
        this.drawBody(ctx);
        
        const blink = this.getBlinkScale();
        this.drawEyes(ctx, this.lookCurrent.x, this.lookCurrent.y, blink);
        
        // Gentle breathing smile
        this.drawMouth(ctx, 0, 0, 8);
        
        // Idle sparkles
        if (Math.random() < 0.05 && this.particles.length < 24) {
            this.particles.push({
                x: (Math.random() - 0.5) * 60,
                y: (Math.random() - 0.5) * 60,
                vx: (Math.random() - 0.5) * 0.5,
                vy: -Math.random() * 0.5 - 0.2,
                life: 0.8,
                decay: 0.008,
                size: 1.5 + Math.random() * 2,
                color: '150, 220, 255',
            });
        }
    }

    drawListening(ctx, now) {
        // Pulsing glow
        const pulse = (Math.sin(now / 200) + 1) / 2;
        ctx.fillStyle = `rgba(100, 200, 255, ${0.2 + pulse * 0.2})`;
        ctx.beginPath();
        ctx.arc(0, 0, 40 + pulse * 5, 0, Math.PI * 2);
        ctx.fill();
        
        this.drawBody(ctx);
        
        // Focused eyes
        this.drawEyes(ctx, 0, -2, 1);
        
        // Waveform mouth
        const time = now / 80;
        const barCount = 5;
        ctx.fillStyle = COLORS.accent;
        for (let i = 0; i < barCount; i++) {
            const height = (Math.sin(time + i * 0.8) * 8 + 8) / 2;
            const x = (i - barCount / 2) * 4;
            ctx.fillRect(x - 1, 6 - height / 2, 2, height);
        }
    }

    drawThinking(ctx, now) {
        this.drawBody(ctx);
        
        // Looking up in thought
        this.drawEyes(ctx, 2, -4, 1);
        
        // Thinking dots
        const dotCount = 3;
        for (let i = 0; i < dotCount; i++) {
            const phase = ((now / 500) + i * 0.3) % 1;
            const alpha = Math.sin(phase * Math.PI);
            ctx.fillStyle = `rgba(255, 255, 255, ${alpha})`;
            ctx.beginPath();
            ctx.arc(-8 + i * 8, 10, 2.5, 0, Math.PI * 2);
            ctx.fill();
        }
        
        // Rotating thinking glow
        ctx.save();
        ctx.rotate(now / 2000);
        ctx.strokeStyle = 'rgba(255, 200, 100, 0.4)';
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.arc(0, 0, 35, 0, Math.PI * 0.5);
        ctx.stroke();
        ctx.beginPath();
        ctx.arc(0, 0, 35, Math.PI, Math.PI * 1.5);
        ctx.stroke();
        ctx.restore();
    }

    drawSpeaking(ctx, now) {
        this.drawBody(ctx);
        
        this.drawEyes(ctx, this.lookCurrent.x * 0.5, this.lookCurrent.y * 0.5, 1);
        
        // Lip sync
        const time = now / 100;
        const openness = Math.abs(Math.sin(time * 2) * Math.sin(time * 0.7));
        this.drawMouth(ctx, openness, 0, 8);
        
        // Sound waves
        const waveCount = 3;
        for (let i = 0; i < waveCount; i++) {
            const phase = ((now / 600) + i * 0.3) % 1;
            const radius = 35 + phase * 20;
            const alpha = (1 - phase) * 0.3;
            ctx.strokeStyle = `rgba(100, 255, 150, ${alpha})`;
            ctx.lineWidth = 2;
            ctx.beginPath();
            ctx.arc(0, 0, radius, -0.3, 0.3);
            ctx.stroke();
            ctx.beginPath();
            ctx.arc(0, 0, radius, Math.PI - 0.3, Math.PI + 0.3);
            ctx.stroke();
        }
    }

    drawAgentic(ctx, now) {
        // Intense glow
        const pulse = (Math.sin(now / 100) + 1) / 2;
        ctx.fillStyle = `rgba(255, 150, 255, ${0.1 + pulse * 0.15})`;
        ctx.beginPath();
        ctx.arc(0, 0, 40 + pulse * 8, 0, Math.PI * 2);
        ctx.fill();
        
        this.drawBody(ctx);
        
        // Tracking eyes with extra movement
        const time = now / 150;
        const lookX = Math.sin(time) * 8 + this.lookCurrent.x * 0.5;
        const lookY = Math.cos(time * 0.8) * 6 + this.lookCurrent.y * 0.5;
        
        // Eyes with glow
        ctx.fillStyle = COLORS.eyesGlow;
        ctx.beginPath();
        ctx.arc(-10, -5, 7, 0, Math.PI * 2);
        ctx.arc(10, -5, 7, 0, Math.PI * 2);
        ctx.fill();
        
        ctx.fillStyle = COLORS.eyes;
        ctx.beginPath();
        ctx.arc(-10 + lookX, -5 + lookY, 4, 0, Math.PI * 2);
        ctx.arc(10 + lookX, -5 + lookY, 4, 0, Math.PI * 2);
        ctx.fill();
        
        // Focused mouth
        ctx.strokeStyle = COLORS.mouth;
        ctx.lineWidth = 1.5;
        ctx.beginPath();
        ctx.moveTo(-3, 8);
        ctx.lineTo(3, 8);
        ctx.stroke();
        
        // Crosshair effect
        ctx.save();
        ctx.globalAlpha = 0.2 + pulse * 0.2;
        ctx.strokeStyle = 'rgba(255, 150, 255, 1)';
        ctx.lineWidth = 1;
        ctx.beginPath();
        ctx.moveTo(-40, 0);
        ctx.lineTo(-35, 0);
        ctx.moveTo(35, 0);
        ctx.lineTo(40, 0);
        ctx.moveTo(0, -40);
        ctx.lineTo(0, -35);
        ctx.moveTo(0, 35);
        ctx.lineTo(0, 40);
        ctx.stroke();
        ctx.restore();
    }

    drawWaking(ctx, now) {
        const elapsed = now - this.stateStart;
        const progress = Math.min(1, elapsed / 1000);
        
        // Expanding rings
        for (let i = 0; i < 3; i++) {
            const phase = ((elapsed / 500) + i * 0.3) % 1;
            const radius = phase * 50;
            const alpha = (1 - phase) * 0.5;
            ctx.strokeStyle = `rgba(255, 255, 100, ${alpha})`;
            ctx.lineWidth = 2;
            ctx.beginPath();
            ctx.arc(0, 0, radius, 0, Math.PI * 2);
            ctx.stroke();
        }
        
        // Scale up from 0.5 to 1
        const scale = 0.5 + progress * 0.5;
        ctx.save();
        ctx.scale(scale, scale);
        
        this.drawBody(ctx);
        
        // Happy waking eyes
        const blink = this.getBlinkScale();
        ctx.save();
        ctx.scale(1, blink);
        
        // ^_^ eyes
        ctx.strokeStyle = COLORS.eyes;
        ctx.lineWidth = 2.5;
        ctx.lineCap = 'round';
        ctx.beginPath();
        ctx.moveTo(-14, -3);
        ctx.lineTo(-10, -7);
        ctx.lineTo(-6, -3);
        ctx.stroke();
        ctx.beginPath();
        ctx.moveTo(6, -3);
        ctx.lineTo(10, -7);
        ctx.lineTo(14, -3);
        ctx.stroke();
        ctx.restore();
        
        // Big smile
        ctx.strokeStyle = COLORS.mouth;
        ctx.lineWidth = 2;
        ctx.beginPath();
        ctx.arc(0, 5, 8, 0.1 * Math.PI, 0.9 * Math.PI);
        ctx.stroke();
        
        ctx.restore();
        ctx.restore();
    }

    drawConfused(ctx, now) {
        this.drawBody(ctx);
        
        // One eye squinting
        ctx.fillStyle = COLORS.eyes;
        ctx.save();
        ctx.scale(1, 0.4);
        ctx.beginPath();
        ctx.arc(-10, -12, 4, 0, Math.PI * 2);
        ctx.fill();
        ctx.restore();
        
        ctx.beginPath();
        ctx.arc(10 + this.lookCurrent.x, -5 + this.lookCurrent.y, 4, 0, Math.PI * 2);
        ctx.fill();
        
        // Wavy mouth
        ctx.strokeStyle = COLORS.mouth;
        ctx.lineWidth = 1.5;
        ctx.beginPath();
        const time = now / 200;
        for (let x = -5; x <= 5; x++) {
            const y = 8 + Math.sin(time + x * 0.5) * 2;
            if (x === -5) ctx.moveTo(x, y);
            else ctx.lineTo(x, y);
        }
        ctx.stroke();
        
        // Question mark
        const bob = Math.sin(now / 300) * 5;
        ctx.fillStyle = 'rgba(255, 255, 100, 0.8)';
        ctx.font = 'bold 20px sans-serif';
        ctx.textAlign = 'center';
        ctx.fillText('?', 20, -20 + bob);
    }

    animate() {
        this.draw();
        requestAnimationFrame(() => this.animate());
    }

    reportReady() {
        const invoke = window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke;
        if (!invoke) {
            console.error('[xenon] __TAURI__.core missing');
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
