export const STATES = Object.freeze({
    IDLE: 'Idle',
    LISTENING: 'Listening',
    THINKING: 'Thinking',
    SPEAKING: 'Speaking',
    AGENTIC: 'Agentic',
});

export const STATE_LIST = Object.freeze([
    STATES.IDLE,
    STATES.LISTENING,
    STATES.THINKING,
    STATES.SPEAKING,
    STATES.AGENTIC,
]);

const POSE_FILES = Object.freeze({
    idle: 'mochi-idle.png',
    walk1: 'mochi-walk-1.png',
    walk2: 'mochi-walk-2.png',
    run1: 'mochi-run-1.png',
    run2: 'mochi-run-2.png',
    sleep: 'mochi-sleep.png',
    jump: 'mochi-jump.png',
    sit: 'mochi-sit.png',
    lookCursor: 'mochi-look-cursor.png',
    hide: 'mochi-hide.png',
    celebrate: 'mochi-celebrate.png',
    eat1: 'mochi-eat-1.png',
    eat2: 'mochi-eat-2.png',
    yawn: 'mochi-yawn.png',
    roll: 'mochi-roll.png',
    blush: 'mochi-blush.png',
    stretch: 'mochi-stretch.png',
    peek: 'mochi-peek.png',
    tiltHead: 'mochi-tilt-head.png',
    shake: 'mochi-shake.png',
    nuzzle: 'mochi-nuzzle.png',
    wiggle: 'mochi-wiggle.png',
    dizzy: 'mochi-dizzy.png',
    surprise: 'mochi-surprise.png',
});

export class Animator {
    constructor() {
        this.images = new Map();
        this.pending = new Map();
        this.poseKeys = new Set();
        // Sprite set in use: 'light' = 320x320 copies, '' = 448x448 originals.
        this.lightSprites = true;
        this.poseDir = '/sprites/light';

        this.state = STATES.IDLE;
        this.stateStart = 0;
        this.prevState = STATES.IDLE;

        this.animPose = 'idle';
        this.animStart = 0;
        this.animDuration = 0;
        this.animLoop = false;
        this.poseOverride = null;

        this.frameIdx = 0;
        this.frameStart = 0;

        this.lastInteraction = 0;
        this.idleBeatStart = 0;
        this.nextIdleBeatIn = 8000 + Math.random() * 12000;
        this.sleeping = false;

        this.tapActive = false;
        this.tapStart = 0;
        this.wakeBeatStart = 0;

        this.level = 0;
        this.speechLevel = 0;
        this.focus = { x: 0.5, y: 0.5 };
        this.focusActive = false;

        this.ready = false;
        this.loadingPromise = this.preloadAll();
    }

    async preloadAll() {
        // Light set: decode only the Idle pose. Every other pose loads on
        // first use (see getPoseImage), so the idle WebContent heap does not
        // hold 27 decoded bitmaps. The full set preloads all of them.
        this.loadPose('idle');
        if (!this.lightSprites) {
            Object.keys(POSE_FILES).forEach((pose) => this.loadPose(pose));
        }
        this.poseKeys = new Set(Object.keys(POSE_FILES));
        this.ready = true;
        return this;
    }

    /** `/sprites/light/mochi-idle.png` or `/sprites/mochi-idle.png`. */
    poseUrl(pose) {
        const file = POSE_FILES[pose] || POSE_FILES.idle;
        return `${this.poseDir}/${file}`;
    }

    /**
     * Switch sprite set at runtime (Settings -> Performance). `true` uses the
     * 320x320 copies in `src/sprites/light/` and decodes rare poses on demand;
     * `false` uses the 448x448 originals in `src/sprites/` and preloads them.
     * Takes effect immediately.
     */
    setLightSprites(light) {
        if (this.lightSprites === light && this.poseDir) return;
        this.lightSprites = light;
        this.poseDir = light ? '/sprites/light' : '/sprites';
        this.images.clear();
        this.pending.clear();
        if (light) {
            this.loadPose(this.animPose || 'idle');
        } else {
            Object.keys(POSE_FILES).forEach((pose) => this.loadPose(pose));
        }
    }

    /** Start decoding one pose if it is not already loaded or loading. */
    loadPose(pose) {
        const url = this.poseUrl(pose);
        if (this.images.has(url) || this.pending.has(url)) return;
        const promise = new Promise((resolve) => {
            const img = new Image();
            img.onload = () => {
                this.images.set(url, img);
                resolve();
            };
            img.onerror = () => resolve();
            img.src = url;
        });
        this.pending.set(url, promise);
    }

    setReady() {
        this.ready = true;
    }

    setState(newState, now = performance.now()) {
        if (!STATE_LIST.includes(newState)) return;
        if (this.state === newState) return;
        this.prevState = this.state;
        this.state = newState;
        this.stateStart = now;
        // Keep a just-triggered wake surprise when transitioning to Listening.
        const keepWakeBeat =
            this.wakeBeatStart > 0 && now - this.wakeBeatStart < 900 &&
            newState === STATES.LISTENING;
        if (!keepWakeBeat) this.poseOverride = null;
        this.tapActive = false;
        this.idleBeatStart = 0;
        this.sleeping = false;

        if (newState === STATES.IDLE) {
            this.lastInteraction = now;
            this.idleBeatStart = now;
            this.nextIdleBeatIn = 8000 + Math.random() * 12000;
            this.startAnim('idle', now, 0, true);
        } else if (newState === STATES.LISTENING) {
            this.startAnim('lookCursor', now, 0, true);
        } else if (newState === STATES.THINKING) {
            this.startAnim('tiltHead', now, 900, true);
        } else if (newState === STATES.SPEAKING) {
            this.startAnim('idle', now, 0, true);
        } else if (newState === STATES.AGENTIC) {
            this.startAnim('lookCursor', now, 0, true);
        }
        this.frameStart = now;
    }

    startAnim(pose, now, duration, loop = false) {
        this.animPose = pose;
        this.animStart = now;
        this.animDuration = duration;
        this.animLoop = loop;
        this.frameIdx = 0;
        this.frameStart = now;
    }

    setLevel(value) {
        const n = typeof value === 'number' ? value : 0;
        this.level = Math.max(0, Math.min(1, n));
    }

    setSpeechLevel(value) {
        const n = typeof value === 'number' ? value : 0;
        this.speechLevel = Math.max(0, Math.min(1, n));
    }

    setFocus(x, y) {
        const cx = typeof x === 'number' && isFinite(x) ? x : 0.5;
        const cy = typeof y === 'number' && isFinite(y) ? y : 0.5;
        this.focus.x = Math.max(0, Math.min(1, cx));
        this.focus.y = Math.max(0, Math.min(1, cy));
        this.focusActive = true;
    }

    clearFocus() {
        this.focusActive = false;
        this.focus.x = 0.5;
        this.focus.y = 0.5;
    }

    noteInteraction(now = performance.now()) {
        this.lastInteraction = now;
        if (this.sleeping) {
            this.sleeping = false;
            this.poseOverride = { pose: 'surprise', start: now, duration: 900, loop: false };
            if (this.state === STATES.IDLE) {
                this.startAnim('surprise', now, 900, false);
            }
        }
    }

    triggerTap(now = performance.now()) {
        this.tapActive = true;
        this.tapStart = now;
        if (this.poseOverride) return;
        const dur = 1200;
        this.poseOverride = {
            pose: Math.random() < 0.5 ? 'celebrate' : 'nuzzle',
            start: now,
            duration: dur,
            loop: false,
        };
    }

    /** Wake word fired: short surprise, then whatever the runtime sets. */
    triggerWake(now = performance.now()) {
        this.wakeBeatStart = now;
        this.noteInteraction(now);
        this.poseOverride = { pose: 'surprise', start: now, duration: 900, loop: false };
    }

    getPoseImage(pose) {
        this.loadPose(pose); // kick off decode; this frame falls back to idle
        return this.images.get(this.poseUrl(pose)) || this.images.get(this.poseUrl('idle')) || null;
    }

    update(now) {
        if (this.state === STATES.IDLE) {
            const idleFor = now - this.idleBeatStart;
            if (!this.sleeping && idleFor > 300000) {
                this.sleeping = true;
                this.poseOverride = { pose: 'sleep', start: now, duration: 0, loop: true };
            } else if (!this.sleeping && !this.poseOverride && idleFor > this.nextIdleBeatIn) {
                const beats = ['yawn', 'stretch', 'sit', 'tiltHead', 'wiggle', 'peek'];
                const beat = beats[Math.floor(Math.random() * beats.length)];
                const dur = 1800 + Math.random() * 1400;
                this.poseOverride = { pose: beat, start: now, duration: dur, loop: false };
                this.idleBeatStart = now;
                this.nextIdleBeatIn = 8000 + Math.random() * 12000;
            }
        }

        if (this.state === STATES.THINKING) {
            const elapsed = now - this.stateStart;
            const cycle = Math.floor(elapsed / 900) % 2;
            const target = cycle === 0 ? 'tiltHead' : 'peek';
            if (this.animPose !== target) {
                this.animPose = target;
                this.animStart = now;
            }
        }

        if (this.tapActive) {
            const elapsed = now - this.tapStart;
            if (elapsed > 1400) {
                this.tapActive = false;
                if (this.poseOverride && (this.poseOverride.pose === 'celebrate' || this.poseOverride.pose === 'nuzzle')) {
                    this.poseOverride = null;
                }
            }
        }

        if (this.poseOverride) {
            const o = this.poseOverride;
            if (o.duration > 0 && now - o.start > o.duration && !o.loop) {
                this.poseOverride = null;
            }
        }

        if (this.state === STATES.SPEAKING) {
            const level = this.speechLevel;
            if (level > 0.65 && this.animPose !== 'surprise') {
                this.animPose = 'surprise';
            } else if (level <= 0.65 && level > 0.2 && this.animPose !== 'blush') {
                this.animPose = 'blush';
            } else if (level <= 0.2 && this.animPose !== 'idle') {
                this.animPose = 'idle';
            }
            if (level > 0.05 && this.animPose !== 'surprise' && Math.floor(now / 260) % 2 === 0) {
                // Alternate between blush and idle via frame timer in getFramePose
            }
        }

        if (this.state === STATES.LISTENING) {
            // gentle lerp of level for waveform smoothing handled in renderer
        }

        const pose = this.poseOverride ? this.poseOverride.pose : this.animPose;
        if (this.frameIdx === 0 || now - this.frameStart > 260) {
            this.frameStart = now;
            this.frameIdx = (this.frameIdx + 1) % 2;
        }
        return pose;
    }

    getFramePose(now) {
        if (this.poseOverride) return this.poseOverride.pose;

        if (this.state === STATES.SPEAKING) {
            const level = this.speechLevel;
            if (level > 0.05) {
                if (level > 0.65) return 'surprise';
                const alt = Math.floor(now / 180) % 2 === 0;
                return alt ? 'blush' : 'idle';
            }
            return 'idle';
        }

        if (this.state === STATES.LISTENING) return 'lookCursor';
        if (this.state === STATES.THINKING) return this.animPose;
        if (this.state === STATES.AGENTIC) return 'lookCursor';
        return 'idle';
    }

    getTransform(now) {
        let dx = 0;
        let dy = 0;
        let scaleX = 1;
        let scaleY = 1;
        let rotation = 0;

        const breathe = Math.sin(now / 1050) * 0.018;
        scaleX += breathe;
        scaleY -= breathe;

        const pose = this.getFramePose(now);

        if (pose === 'sleep') {
            const snore = (Math.sin(now / 900) + 1) / 2;
            scaleY += snore * 0.03;
            scaleX -= snore * 0.02;
            dy += snore * 2.5;
        } else if (pose === 'jump') {
            const hop = Math.abs(Math.sin(now / 280));
            dy -= hop * 14;
            if (hop > 0.1) {
                scaleY += 0.06;
                scaleX -= 0.04;
            }
        } else if (pose === 'celebrate') {
            rotation = Math.sin(now / 160) * 0.06;
            dy -= Math.abs(Math.sin(now / 260)) * 5;
        } else if (pose === 'nuzzle') {
            rotation = Math.sin(now / 380) * 0.05;
            dx += 3;
            dy += 2;
        } else if (pose === 'yawn') {
            const stretch = Math.sin((now - (this.poseOverride?.start || 0)) / 700 * Math.PI);
            scaleY += Math.max(0, stretch) * 0.08;
            scaleX -= Math.max(0, stretch) * 0.04;
            dy -= Math.max(0, stretch) * 4;
        } else if (pose === 'stretch') {
            const stretch = Math.sin((now - (this.poseOverride?.start || 0)) / 800 * Math.PI);
            scaleY += Math.max(0, stretch) * 0.1;
            scaleX -= Math.max(0, stretch) * 0.05;
        } else if (pose === 'sit') {
            dy += 3;
            scaleY -= 0.02;
        } else if (pose === 'tiltHead') {
            rotation = -0.08 + Math.sin(now / 500) * 0.03;
        } else if (pose === 'peek') {
            dx += Math.sin(now / 600) * 4;
            rotation = Math.sin(now / 700) * 0.04;
        } else if (pose === 'wiggle') {
            rotation = Math.sin(now / 120) * 0.05;
        } else if (pose === 'shake') {
            dx += Math.sin(now / 50) * 3;
        } else if (pose === 'surprise') {
            const pop = Math.max(0, Math.sin((now - (this.tapActive ? this.tapStart : now)) / 280 * Math.PI));
            scaleX += pop * 0.07;
            scaleY += pop * 0.07;
            dy -= pop * 3;
        }

        if (this.state === STATES.THINKING) {
            const pulse = (Math.sin(now / 320) + 1) / 2;
            scaleX += pulse * 0.01;
            scaleY += pulse * 0.01;
        }

        if (this.state === STATES.AGENTIC && this.focusActive) {
            const leanX = (this.focus.x - 0.5) * 8;
            const leanY = (this.focus.y - 0.5) * 6;
            dx += leanX;
            dy += leanY;
            rotation += (this.focus.x - 0.5) * 0.12;
        }

        if (this.state === STATES.SPEAKING) {
            const level = this.speechLevel;
            if (level > 0.05) {
                const squash = Math.sin(now / 90) * 0.035 * level;
                scaleY += squash;
                scaleX -= squash * 0.7;
            }
        }

        return { dx, dy, scaleX, scaleY, rotation, pose };
    }

    getBobOffset(now) {
        return Math.sin(now / 900) * 3;
    }
}

export function pickIdleBeatPose() {
    const beats = ['yawn', 'stretch', 'sit', 'tiltHead', 'wiggle', 'peek'];
    return beats[Math.floor(Math.random() * beats.length)];
}