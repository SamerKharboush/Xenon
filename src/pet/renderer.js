import { Animator, STATES, STATE_LIST } from './animator.js';
import { Bubble, BUBBLE_COLORS } from './bubble.js';

const PET_W = 320;
const PET_H = 300;

const ACCENT = '255, 122, 161';
const THINKING = '255, 200, 120';
const AGENTIC = '180, 150, 255';
const CREAM = '255, 243, 224';

class Renderer {
    constructor() {
        this.canvas = document.getElementById('canvas');
        this.ctx = this.canvas.getContext('2d');
        this.animator = new Animator();
        this.bubble = new Bubble();
        this.lastTime = performance.now();
        this.dpr = 1;
        this.idleLevel = 0;
        this.speechLevelSmooth = 0;
        this.audioLevelSmooth = 0;
        this.waveBars = new Array(24).fill(0);
        this.orbitAngle = 0;
        this.lastOpaqueReport = 0;
        this.poseSignature = '';
        this.lastPoseReport = '';
        this.particles = [];
        this.sparkleTimer = 0;
        this.menuOpen = false;
        this.menuX = 0;
        this.menuY = 0;
        this.menuHovered = -1;
        this.dragState = null;
        this.dragThreshold = 4;
        this.dragStarted = false;
        this.clickTimer = null;
        this.lastClickTime = 0;
        this.voiceMuted = false;

        this.resize();
        window.addEventListener('resize', () => this.resize());

        this.initTauri();
        this.initInteractions();

        this.animator.loadingPromise.then(() => {
            this.animator.setReady();
            // Delay probe to ensure at least one frame has been drawn.
            setTimeout(() => this.reportReady(), 150);
        });
        this.rafId = null;
        this.loop = this.loop.bind(this);
        this.rafId = requestAnimationFrame(this.loop);
    }

    resize() {
        this.dpr = window.devicePixelRatio || 1;
        const w = window.innerWidth || PET_W;
        const h = window.innerHeight || PET_H;
        this.canvas.width = Math.round(w * this.dpr);
        this.canvas.height = Math.round(h * this.dpr);
        this.canvas.style.width = w + 'px';
        this.canvas.style.height = h + 'px';
        this.ctx.setTransform(this.dpr, 0, 0, this.dpr, 0, 0);
        this.cssW = w;
        this.cssH = h;
    }

    initTauri() {
        const tauri = window.__TAURI__;
        if (!tauri || !tauri.event || !tauri.core) return;
        const { listen } = tauri.event;
        const { invoke } = tauri.core;
        this.invoke = invoke;

        listen('pet-state', (event) => {
            this.animator.setState(event.payload);
        });

        listen('pet-speech', (event) => {
            this.bubble.show(event.payload, BUBBLE_COLORS.speech, 0);
        });

        listen('audio-level', (event) => {
            const raw = event.payload;
            const v = typeof raw === 'number' ? raw : raw && typeof raw.rms === 'number' ? raw.rms : 0;
            this.animator.setLevel(v);
        });

        listen('speech-level', (event) => {
            const raw = event.payload;
            const v = typeof raw === 'number' ? raw : raw && typeof raw.rms === 'number' ? raw.rms : 0;
            this.animator.setSpeechLevel(v);
            if (v > 0.02 && this.animator.state === STATES.SPEAKING) {
                this.bubble.hideAfter = 0;
            }
        });

        listen('agent-focus', (event) => {
            const p = event.payload || {};
            const x = typeof p.x === 'number' ? p.x : 0.5;
            const y = typeof p.y === 'number' ? p.y : 0.5;
            this.animator.setFocus(x, y);
        });

        listen('xenon-notice', (event) => {
            const raw = event.payload;
            const text = typeof raw === 'string' ? raw : raw && typeof raw.message === 'string' ? raw.message : String(raw);
            this.bubble.show(text, BUBBLE_COLORS.notice, 4000);
        });
    }

    initInteractions() {
        const canvas = this.canvas;
        canvas.style.touchAction = 'none';

        canvas.addEventListener('pointerdown', (e) => {
            if (e.button === 2) return;
            if (this.menuOpen) {
                this.handleMenuClick(e);
                return;
            }
            this.dragState = { x: e.clientX, y: e.clientY, pointerId: e.pointerId };
            this.dragStarted = false;
            try { canvas.setPointerCapture(e.pointerId); } catch (_) {}
        });

        canvas.addEventListener('pointermove', (e) => {
            if (this.menuOpen) {
                this.updateMenuHover(e);
                return;
            }
            if (!this.dragState) return;
            const dx = e.clientX - this.dragState.x;
            const dy = e.clientY - this.dragState.y;
            if (!this.dragStarted && Math.hypot(dx, dy) > this.dragThreshold) {
                this.dragStarted = true;
                const tauri = window.__TAURI__;
                try {
                    canvas.releasePointerCapture(this.dragState.pointerId);
                } catch (_) {}
                if (tauri && tauri.window && tauri.window.getCurrentWindow) {
                    tauri.window.getCurrentWindow().startDragging().catch(() => {});
                } else if (this.invoke) {
                    this.invoke('start_window_drag').catch(() => {});
                }
            }
        });

        canvas.addEventListener('pointerup', (e) => {
            if (this.menuOpen) {
                this.menuOpen = false;
                return;
            }
            if (!this.dragState) return;
            try { canvas.releasePointerCapture(e.pointerId); } catch (_) {}
            const wasDrag = this.dragStarted;
            this.dragState = null;
            this.dragStarted = false;
            if (wasDrag) return;
            this.handleTap(e);
        });

        canvas.addEventListener('pointercancel', () => {
            this.dragState = null;
            this.dragStarted = false;
        });

        canvas.addEventListener('contextmenu', (e) => {
            e.preventDefault();
            if (this.menuOpen) {
                this.menuOpen = false;
                return;
            }
            const rect = canvas.getBoundingClientRect();
            this.menuX = Math.min(this.cssW - 150, Math.max(4, e.clientX - rect.left));
            this.menuY = Math.min(this.cssH - 110, Math.max(4, e.clientY - rect.top));
            this.menuOpen = true;
            this.menuHovered = -1;
        });
    }

    handleTap(e) {
        const now = performance.now();
        this.animator.noteInteraction(now);
        this.animator.triggerTap(now);

        if (now - this.lastClickTime < 320) {
            this.lastClickTime = 0;
            if (this.invoke) this.invoke('open_settings').catch(() => {});
            return;
        }
        this.lastClickTime = now;

        if (this.clickTimer) clearTimeout(this.clickTimer);
        this.clickTimer = setTimeout(() => {
            this.clickTimer = null;
            if (this.invoke) {
                this.invoke('pet_tap').catch(() => {});
                if (this.voiceMuted) {
                    this.invoke('stop_speaking').catch((err) => {
                        const msg = String(err || '');
                        if (!msg.includes('not found') && !msg.includes('command')) {
                            console.warn('[xenon] stop_speaking:', err);
                        }
                    });
                }
            }
        }, 230);
    }

    handleMenuClick(e) {
        const rect = this.canvas.getBoundingClientRect();
        const x = e.clientX - rect.left;
        const y = e.clientY - rect.top;
        const items = this.getMenuItems();
        for (let i = 0; i < items.length; i++) {
            const item = items[i];
            if (x >= item.x && x <= item.x + item.w && y >= item.y && y <= item.y + 22) {
                this.menuOpen = false;
                this.activateMenuItem(i);
                return;
            }
        }
        this.menuOpen = false;
    }

    updateMenuHover(e) {
        const rect = this.canvas.getBoundingClientRect();
        const x = e.clientX - rect.left;
        const y = e.clientY - rect.top;
        const items = this.getMenuItems();
        this.menuHovered = -1;
        for (let i = 0; i < items.length; i++) {
            const item = items[i];
            if (x >= item.x && x <= item.x + item.w && y >= item.y && y <= item.y + 22) {
                this.menuHovered = i;
                break;
            }
        }
    }

    getMenuItems() {
        const mx = Math.round(this.menuX);
        const my = Math.round(this.menuY);
        const w = 140;
        return [
            { label: 'Settings', x: mx, y: my, w, action: 'settings' },
            { label: 'Talk', x: mx, y: my + 26, w, action: 'talk' },
            { label: this.voiceMuted ? 'Unmute voice' : 'Mute voice', x: mx, y: my + 52, w, action: 'mute' },
            { label: 'Quit', x: mx, y: my + 78, w, action: 'quit' },
        ];
    }

    activateMenuItem(index) {
        const item = this.getMenuItems()[index];
        if (!item) return;
        const invoke = this.invoke;
        if (!invoke) return;
        if (item.action === 'settings') {
            invoke('open_settings').catch(() => {});
        } else if (item.action === 'talk') {
            invoke('pet_tap').catch(() => {});
        } else if (item.action === 'mute') {
            this.voiceMuted = !this.voiceMuted;
            if (this.voiceMuted) {
                invoke('stop_speaking').catch((err) => {
                    const msg = String(err || '');
                    if (!msg.includes('not found') && !msg.includes('command')) {
                        console.warn('[xenon] stop_speaking:', err);
                    }
                });
            }
        } else if (item.action === 'quit') {
            const tauri = window.__TAURI__;
            if (tauri && tauri.app) {
                tauri.app.exit(0).catch(() => {});
            }
        }
    }

    loop(now) {
        const dt = Math.min(48, now - this.lastTime);
        this.lastTime = now;

        const anim = this.animator;
        const state = anim.state;

        this.idleLevel += (anim.level - this.idleLevel) * 0.18;
        this.speechLevelSmooth += (anim.speechLevel - this.speechLevelSmooth) * 0.3;
        this.audioLevelSmooth += (anim.level - this.audioLevelSmooth) * 0.15;

        anim.update(now);
        this.updateParticles(now, dt, state);
        this.draw(now, state);

        const pose = anim.getFramePose(now);
        if (pose !== this.poseSignature) {
            this.poseSignature = pose;
            const nowMs = performance.now();
            if (nowMs - this.lastOpaqueReport > 250) {
                this.reportReady();
                this.lastOpaqueReport = nowMs;
            }
        }

        this.bubble.update(now);
        this.rafId = requestAnimationFrame(this.loop);
    }

    updateParticles(now, dt, state) {
        const anim = this.animator;
        const cx = this.cssW / 2;
        const petBottom = this.getPetBottom();
        const petTop = petBottom - this.getPetSize();
        const petMid = (petTop + petBottom) / 2;

        if (state === STATES.THINKING && Math.random() < 0.03) {
            this.spawnOrbitDot(now, cx, petMid, THINKING);
        }
        if (state === STATES.AGENTIC && Math.random() < 0.045) {
            this.spawnSparkle(now, cx, petMid, AGENTIC);
        }
        if (state === STATES.LISTENING && Math.random() < 0.02) {
            this.spawnSparkle(now, cx, petMid, ACCENT);
        }
        if (anim.sleeping && Math.random() < 0.015) {
            this.spawnZ(now, cx, petTop);
        }

        const dtSec = dt / 1000;
        for (const p of this.particles) {
            p.life -= p.decay * dtSec;
            p.x += p.vx * dtSec;
            p.y += p.vy * dtSec;
            if (p.gravity) p.vy += p.gravity * dtSec;
        }
        this.particles = this.particles.filter((p) => p.life > 0);
    }

    spawnOrbitDot(now, cx, cy, color) {
        const angle = this.orbitAngle;
        const r = 70;
        this.particles.push({
            kind: 'dot',
            x: cx + Math.cos(angle) * r,
            y: cy + Math.sin(angle) * r * 0.5,
            vx: 0,
            vy: 0,
            life: 0.9,
            decay: 0.5,
            size: 2.4,
            color,
        });
    }

    spawnSparkle(now, cx, cy, color) {
        const angle = Math.random() * Math.PI * 2;
        const r = 40 + Math.random() * 50;
        this.particles.push({
            kind: 'sparkle',
            x: cx + Math.cos(angle) * r,
            y: cy + Math.sin(angle) * r * 0.7,
            vx: (Math.random() - 0.5) * 8,
            vy: -10 - Math.random() * 14,
            life: 0.9,
            decay: 0.9,
            size: 2 + Math.random() * 2.5,
            color,
        });
    }

    spawnZ(now, cx, petTop) {
        this.particles.push({
            kind: 'z',
            x: cx + 30 + Math.random() * 20,
            y: petTop + 20,
            vx: 4 + Math.random() * 6,
            vy: -14 - Math.random() * 8,
            life: 2.2,
            decay: 0.45,
            size: 10 + Math.random() * 6,
            color: '107, 159, 196',
        });
    }

    draw(now, state) {
        const ctx = this.ctx;
        ctx.clearRect(0, 0, this.cssW, this.cssH);

        const cx = this.cssW / 2;
        const petSize = this.getPetSize();
        const petBottom = this.getPetBottom();
        const petTop = petBottom - petSize;

        const transform = this.animator.getTransform(now);
        const pose = transform.pose;

        const bob = this.animator.getBobOffset(now);
        const dx = transform.dx;
        const dy = transform.dy + bob;

        ctx.save();
        ctx.translate(cx + dx, petBottom + dy);
        ctx.scale(transform.scaleX, transform.scaleY);
        ctx.rotate(transform.rotation);
        ctx.translate(-cx, -petBottom);

        if (state === STATES.LISTENING) {
            this.drawListeningRing(ctx, now, cx, petTop + petSize * 0.45, this.audioLevelSmooth);
        }
        if (state === STATES.THINKING) {
            this.drawThinkingGlow(ctx, now, cx, petTop + petSize * 0.45);
        }
        if (state === STATES.SPEAKING) {
            this.drawSpeakingAura(ctx, now, cx, petTop + petSize * 0.45, this.speechLevelSmooth);
        }

        const img = this.animator.getPoseImage(pose);
        if (img) {
            ctx.drawImage(img, cx - petSize / 2, petTop, petSize, petSize);
        } else {
            this.drawFallback(ctx, cx, petTop, petSize);
        }

        ctx.restore();

        this.drawParticles(ctx);
        this.bubble.update(now);
        this.bubble.draw(ctx, cx, petTop + 8);

        if (this.menuOpen) {
            this.drawMenu(ctx);
        }
    }

    getPetSize() {
        return 240;
    }

    getPetBottom() {
        return this.cssH - 30;
    }

    drawListeningRing(ctx, now, cx, cy, level) {
        const baseR = 72;
        const pulse = 1 + Math.sin(now / 220) * 0.04 + level * 0.08;
        const r = baseR * pulse;

        ctx.save();
        ctx.lineWidth = 3;
        ctx.lineCap = 'round';

        const segments = 48;
        for (let i = 0; i < segments; i++) {
            const angle = (i / segments) * Math.PI * 2 - Math.PI / 2;
            const wave = Math.sin(now / 120 + i * 0.55) * 0.5 + 0.5;
            const h = 6 + wave * 18 * (0.3 + level * 0.7);
            const inner = r;
            const outer = r + h;
            const cos = Math.cos(angle);
            const sin = Math.sin(angle);
            const alpha = 0.35 + wave * 0.5;
            ctx.strokeStyle = `rgba(${ACCENT}, ${alpha})`;
            ctx.beginPath();
            ctx.moveTo(cx + cos * inner, cy + sin * inner);
            ctx.lineTo(cx + cos * outer, cy + sin * outer);
            ctx.stroke();
        }
        ctx.restore();
    }

    drawThinkingGlow(ctx, now, cx, cy) {
        const pulse = (Math.sin(now / 320) + 1) / 2;
        ctx.save();
        const grad = ctx.createRadialGradient(cx, cy, 20, cx, cy, 95 + pulse * 8);
        grad.addColorStop(0, `rgba(${THINKING}, 0)`);
        grad.addColorStop(0.6, `rgba(${THINKING}, ${0.06 + pulse * 0.07})`);
        grad.addColorStop(1, `rgba(${THINKING}, 0)`);
        ctx.fillStyle = grad;
        ctx.fillRect(0, 0, this.cssW, this.cssH);
        ctx.restore();
    }

    drawSpeakingAura(ctx, now, cx, cy, level) {
        const waveCount = 3;
        ctx.save();
        ctx.lineWidth = 2;
        for (let i = 0; i < waveCount; i++) {
            const phase = ((now / 500) + i / waveCount) % 1;
            const r = 64 + phase * 36;
            const alpha = (1 - phase) * 0.22;
            ctx.strokeStyle = `rgba(${CREAM}, ${alpha})`;
            ctx.beginPath();
            ctx.arc(cx, cy, r, -0.4, 0.4);
            ctx.stroke();
            ctx.beginPath();
            ctx.arc(cx, cy, r, Math.PI - 0.4, Math.PI + 0.4);
            ctx.stroke();
        }
        if (level > 0.1) {
            const grad = ctx.createRadialGradient(cx, cy, 30, cx, cy, 100);
            grad.addColorStop(0, `rgba(${ACCENT}, 0)`);
            grad.addColorStop(0.7, `rgba(${ACCENT}, ${level * 0.07})`);
            grad.addColorStop(1, `rgba(${ACCENT}, 0)`);
            ctx.fillStyle = grad;
            ctx.fillRect(0, 0, this.cssW, this.cssH);
        }
        ctx.restore();
    }

    drawParticles(ctx) {
        for (const p of this.particles) {
            const alpha = Math.max(0, p.life);
            if (p.kind === 'z') {
                ctx.save();
                ctx.globalAlpha = alpha;
                ctx.fillStyle = `rgba(${p.color}, 1)`;
                ctx.font = `bold ${p.size}px -apple-system, sans-serif`;
                ctx.fillText('z', p.x, p.y);
                ctx.restore();
            } else if (p.kind === 'sparkle') {
                ctx.save();
                ctx.globalAlpha = alpha;
                ctx.fillStyle = `rgba(${p.color}, ${alpha})`;
                ctx.beginPath();
                ctx.arc(p.x, p.y, p.size * alpha, 0, Math.PI * 2);
                ctx.fill();
                ctx.restore();
            } else {
                ctx.save();
                ctx.globalAlpha = alpha;
                ctx.fillStyle = `rgba(${p.color}, ${alpha})`;
                ctx.beginPath();
                ctx.arc(p.x, p.y, p.size, 0, Math.PI * 2);
                ctx.fill();
                ctx.restore();
            }
        }
    }

    drawMenu(ctx) {
        const items = this.getMenuItems();
        const mx = Math.round(this.menuX);
        const my = Math.round(this.menuY);
        const w = 140;
        const h = items.length * 26 + 8;

        ctx.save();
        ctx.fillStyle = 'rgba(255, 247, 236, 0.98)';
        roundRect(ctx, mx, my, w, h, 10);
        ctx.fill();
        ctx.strokeStyle = 'rgba(255, 122, 161, 0.5)';
        ctx.lineWidth = 1;
        ctx.stroke();

        ctx.font = '12px -apple-system, BlinkMacSystemFont, sans-serif';
        ctx.textBaseline = 'middle';
        for (let i = 0; i < items.length; i++) {
            const item = items[i];
            if (i === this.menuHovered) {
                ctx.fillStyle = 'rgba(255, 122, 161, 0.18)';
                ctx.fillRect(mx + 4, item.y, w - 8, 22);
            }
            ctx.fillStyle = '#3a2b34';
            ctx.textAlign = 'left';
            ctx.fillText(item.label, mx + 12, item.y + 11);
        }
        ctx.restore();
    }

    drawFallback(ctx, cx, top, size) {
        ctx.save();
        ctx.fillStyle = 'rgba(255, 198, 211, 0.9)';
        ctx.beginPath();
        ctx.arc(cx, top + size / 2, size * 0.35, 0, Math.PI * 2);
        ctx.fill();
        ctx.fillStyle = '#3a2b34';
        ctx.beginPath();
        ctx.arc(cx - 14, top + size * 0.42, 4, 0, Math.PI * 2);
        ctx.arc(cx + 14, top + size * 0.42, 4, 0, Math.PI * 2);
        ctx.fill();
        ctx.restore();
    }

    reportReady() {
        const invoke = this.invoke || (window.__TAURI__ && window.__TAURI__.core && window.__TAURI__.core.invoke);
        if (!invoke) {
            console.error('[xenon] __TAURI__.core missing');
            return;
        }
        let opaque = 0;
        try {
            const data = this.ctx.getImageData(0, 0, this.canvas.width, this.canvas.height).data;
            // Sample every 4th pixel on a grid; enough for a >0 signal.
            for (let i = 3; i < data.length; i += 16) {
                if (data[i] > 0) opaque++;
            }
            opaque = opaque * 4;
        } catch (e) {
            opaque = 0;
        }
        invoke('frontend_ready', {
            report: {
                width: this.canvas.width,
                height: this.canvas.height,
                opaquePixels: opaque,
            }
        }).catch((e) => console.error('[xenon] frontend_ready failed', e));
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

function installDemo() {
    const states = STATE_LIST;
    let idx = 0;
    let demoTimer = null;
    window.__xenonDemo = function demo() {
        const renderer = window.__xenonRenderer;
        if (!renderer) return;
        if (demoTimer) clearInterval(demoTimer);
        let n = 0;
        const step = () => {
            const s = states[idx % states.length];
            idx++;
            renderer.animator.setState(s);
            if (s === STATES.LISTENING) {
                let v = 0;
                const t = setInterval(() => {
                    v = Math.abs(Math.sin(Date.now() / 180)) * 0.8;
                    renderer.animator.setLevel(v);
                }, 40);
                setTimeout(() => clearInterval(t), 2600);
            }
            if (s === STATES.SPEAKING) {
                renderer.bubble.show('Hello! I am Xenon, your desktop companion. This is a demo of the speech bubble and lip sync.', BUBBLE_COLORS.speech, 0);
                let v = 0;
                const t = setInterval(() => {
                    v = Math.abs(Math.sin(Date.now() / 120) * Math.cos(Date.now() / 300)) * 0.85;
                    renderer.animator.setSpeechLevel(v);
                }, 40);
                setTimeout(() => clearInterval(t), 2600);
            }
            if (s === STATES.AGENTIC) {
                let a = 0;
                const t = setInterval(() => {
                    a += 0.08;
                    renderer.animator.setFocus(0.5 + Math.cos(a) * 0.4, 0.5 + Math.sin(a) * 0.4);
                }, 40);
                setTimeout(() => clearInterval(t), 2600);
            }
            if (s === STATES.THINKING) {
                // handled by animator alternation
            }
            n++;
            if (n >= states.length) {
                clearInterval(demoTimer);
                demoTimer = null;
                setTimeout(() => renderer.animator.setState(STATES.IDLE), 2800);
            }
        };
        step();
        demoTimer = setInterval(step, 2800);
    };
}

if (typeof window !== 'undefined') {
    window.addEventListener('DOMContentLoaded', () => {
        const renderer = new Renderer();
        window.__xenonRenderer = renderer;
        installDemo();
    });
}