/**
 * The extension's side of aurix-hub, the always-on front door of aurix-dbg: its REST API,
 * starting the hub of this machine, the install/uninstall commands, the hub's event stream and a
 * local TCP port that relays TCF over the hub's WebSocket (for a hub on another machine).
 *
 * No dependencies beyond Node: the HTTP client is `http`, the WebSocket client a minimal one
 * (binary frames, client masking, ping/pong, close) good for the relay.
 */
import * as http from 'http';
import * as net from 'net';
import * as path from 'path';
import * as fs from 'fs';
import * as crypto from 'crypto';
import { execFile, spawn } from 'child_process';

export const DEFAULT_HUB_URL = 'http://127.0.0.1:3001';

/** A failed hub request, with the API error code when the hub sent one. */
export class HubError extends Error {
	constructor(message: string, readonly status: number, readonly code?: string, readonly body?: any) {
		super(message);
	}
}

export interface LaunchStep { step: string; ok: boolean; ms: number; detail?: string }
export interface LaunchStatus { phase: 'idle' | 'starting' | 'ready' | 'failed'; steps: LaunchStep[]; error?: string; targets?: any[] }
export interface SessionInfo {
	session: string;
	name: string;
	profile?: string;
	state: 'starting' | 'running' | 'exited';
	pid?: number;
	opened_by?: string;
	adapter?: string;
	targets: string[];
	tcf_port?: number;
	telnet_port?: number;
	base_url: string;
	control?: string;
	launch: LaunchStatus;
	log_file: string;
	error?: string;
}

export interface HubOptions {
	url: string;
	token?: string;
	/** `X-Aurix-Client`: owner of this client's breakpoints and lease. */
	clientId: string;
}

/** Whether a hub URL is this machine. */
export function isLocalUrl(url: string): boolean {
	try {
		const h = new URL(url).hostname;
		return h === '127.0.0.1' || h === 'localhost' || h === '::1' || h === '[::1]';
	} catch {
		return false;
	}
}

export class HubClient {
	constructor(readonly opts: HubOptions) {}

	get local(): boolean {
		return isLocalUrl(this.opts.url);
	}

	private headers(extra?: Record<string, string>): Record<string, string> {
		const h: Record<string, string> = { 'X-Aurix-Client': this.opts.clientId, ...extra };
		if (this.opts.token) {
			h['Authorization'] = `Bearer ${this.opts.token}`;
		}
		return h;
	}

	/** One request; resolves with the JSON body, rejects with a HubError. */
	request(method: string, p: string, body?: unknown, timeoutMs = 3_700_000): Promise<any> {
		const url = new URL(p, this.opts.url);
		const data = body === undefined ? undefined : Buffer.from(JSON.stringify(body));
		return new Promise((resolve, reject) => {
			const req = http.request(url, {
				method,
				headers: this.headers(data ? { 'Content-Type': 'application/json', 'Content-Length': String(data.length) } : {}),
				timeout: timeoutMs,
			}, (res) => {
				const chunks: Buffer[] = [];
				res.on('data', (c: Buffer) => chunks.push(c));
				res.on('end', () => {
					const text = Buffer.concat(chunks).toString('utf8');
					let json: any = undefined;
					try {
						json = text ? JSON.parse(text) : undefined;
					} catch {
						// not JSON
					}
					const status = res.statusCode ?? 0;
					if (status >= 200 && status < 300) {
						resolve(json);
					} else {
						reject(new HubError(json?.message ?? (text || `HTTP ${status}`), status, json?.code, json));
					}
				});
			});
			req.on('timeout', () => req.destroy(new Error(`no answer from ${url.origin} within ${Math.round(timeoutMs / 1000)} s`)));
			req.on('error', (e) => reject(new HubError(`aurix-hub at ${this.opts.url} is not reachable: ${e.message}`, 0)));
			if (data) {
				req.write(data);
			}
			req.end();
		});
	}

	async health(): Promise<any | undefined> {
		try {
			const h = await this.request('GET', '/health', undefined, 3000);
			return h?.role === 'aurix-hub' ? h : undefined;
		} catch {
			return undefined;
		}
	}

	openSession(req: unknown): Promise<SessionInfo> {
		return this.request('POST', '/api/v1/sessions', req);
	}

	async listSessions(): Promise<SessionInfo[]> {
		return (await this.request('GET', '/api/v1/sessions', undefined, 30_000)).sessions ?? [];
	}

	async findSession(key: string): Promise<SessionInfo | undefined> {
		const all = await this.listSessions();
		return all.find((s) => s.session === key) ?? all.find((s) => s.name === key && s.state !== 'exited');
	}

	closeSession(key: string): Promise<unknown> {
		return this.request('DELETE', `/api/v1/sessions/${encodeURIComponent(key)}`, undefined, 30_000);
	}

	waitSession(key: string, timeoutS: number): Promise<LaunchStatus> {
		return this.request('GET', `/api/v1/sessions/${encodeURIComponent(key)}/launch?timeout_s=${timeoutS}`);
	}

	sessionLog(key: string, lines = 40): Promise<{ text: string; file: string }> {
		return this.request('GET', `/api/v1/sessions/${encodeURIComponent(key)}/log?lines=${lines}`, undefined, 10_000);
	}

	endClient(key: string): Promise<unknown> {
		return this.request('POST', `/api/v1/sessions/${encodeURIComponent(key)}/clients/end`, { client: this.opts.clientId }, 10_000);
	}

	/** Uploads a local file to the hub machine; returns its `file:` ID. */
	upload(file: string): Promise<string> {
		const url = new URL(`/api/v1/files?name=${encodeURIComponent(path.basename(file))}`, this.opts.url);
		const data = fs.readFileSync(file);
		return new Promise((resolve, reject) => {
			const req = http.request(url, {
				method: 'POST',
				headers: this.headers({ 'Content-Type': 'application/octet-stream', 'Content-Length': String(data.length) }),
			}, (res) => {
				const chunks: Buffer[] = [];
				res.on('data', (c: Buffer) => chunks.push(c));
				res.on('end', () => {
					try {
						const v = JSON.parse(Buffer.concat(chunks).toString('utf8'));
						if ((res.statusCode ?? 0) < 300 && typeof v.file === 'string') {
							resolve(v.file);
							return;
						}
						reject(new HubError(v.message ?? `upload of ${file} failed`, res.statusCode ?? 0));
					} catch (e) {
						reject(e);
					}
				});
			});
			req.on('error', reject);
			req.end(data);
		});
	}

	/**
	 * Follows the hub's event stream (`session_opened`, ...), reconnecting every 10 s while the
	 * hub is away. Returns a function that stops it.
	 */
	followEvents(onEvent: (ev: any) => void): () => void {
		let stopped = false;
		let current: http.ClientRequest | undefined;
		const connect = () => {
			if (stopped) {
				return;
			}
			const url = new URL('/api/v1/events', this.opts.url);
			current = http.get(url, { headers: this.headers({ Accept: 'text/event-stream' }) }, (res) => {
				let buf = '';
				res.setEncoding('utf8');
				res.on('data', (chunk: string) => {
					buf += chunk;
					let i;
					while ((i = buf.indexOf('\n\n')) >= 0) {
						const block = buf.slice(0, i);
						buf = buf.slice(i + 2);
						const data = block.split('\n').filter((l) => l.startsWith('data:')).map((l) => l.slice(5).trim()).join('\n');
						if (data) {
							try {
								onEvent(JSON.parse(data));
							} catch {
								// ignore
							}
						}
					}
				});
				res.on('end', () => setTimeout(connect, 10_000));
			});
			current.on('error', () => setTimeout(connect, 10_000));
		};
		connect();
		return () => {
			stopped = true;
			current?.destroy();
		};
	}
}

/** `%LOCALAPPDATA%\aurix-dbg\current\aurix-hub.exe`: the installed hub (fixed path). */
export function installedHubExe(): string {
	const base = process.env.LOCALAPPDATA ?? path.join(process.env.USERPROFILE ?? '', 'AppData', 'Local');
	return path.join(base, 'aurix-dbg', 'current', 'aurix-hub.exe');
}

/** The hub bundled with the extension. */
export function bundledHubExe(extensionPath: string): string {
	return path.join(extensionPath, 'bin', 'aurix-hub.exe');
}

/** The hub to start: the installed one, else the bundled one. */
export function hubExe(extensionPath: string): string {
	const installed = installedHubExe();
	return fs.existsSync(installed) ? installed : bundledHubExe(extensionPath);
}

/** Starts the hub of this machine unless one answers; waits until it does. */
export async function ensureLocalHub(client: HubClient, extensionPath: string, log: (l: string) => void): Promise<void> {
	if (await client.health()) {
		return;
	}
	const exe = hubExe(extensionPath);
	if (!fs.existsSync(exe)) {
		throw new Error(`aurix-hub not found: ${exe}`);
	}
	log(`Starting aurix-hub: ${exe}`);
	// The hub detaches itself (no inherited handles, no console); this child exits at once.
	const child = spawn(exe, ['serve'], { detached: true, stdio: 'ignore', windowsHide: true });
	child.on('error', (e) => log(`Failed to start aurix-hub: ${e.message}`));
	child.unref();
	const t0 = Date.now();
	while (Date.now() - t0 < 15_000) {
		if (await client.health()) {
			log('aurix-hub is running');
			return;
		}
		await new Promise((r) => setTimeout(r, 200));
	}
	throw new Error(`aurix-hub did not start within 15 s (see %LOCALAPPDATA%\\aurix-dbg\\logs\\hub.log)`);
}

/** Runs `aurix-hub <args>` (install, uninstall, status); resolves with its output. */
export function runHub(exe: string, args: string[], timeoutMs = 120_000): Promise<{ code: number; out: string }> {
	return new Promise((resolve) => {
		execFile(exe, args, { windowsHide: true, timeout: timeoutMs }, (err, stdout, stderr) => {
			const code = err ? (typeof (err as any).code === 'number' ? (err as any).code : 1) : 0;
			resolve({ code, out: `${stdout ?? ''}${stderr ?? ''}`.trim() });
		});
	});
}

// --- TCF over the hub's WebSocket --------------------------------------------------------------

/** A minimal WebSocket client connection: binary messages in both directions. */
class WsConnection {
	private buf = Buffer.alloc(0);
	private closed = false;
	onMessage: (data: Buffer) => void = () => {};
	onClose: () => void = () => {};

	constructor(private readonly socket: net.Socket, head: Buffer) {
		socket.on('data', (d: Buffer) => this.feed(d));
		socket.on('close', () => this.finish());
		socket.on('error', () => this.finish());
		if (head.length) {
			this.feed(head);
		}
	}

	private finish(): void {
		if (!this.closed) {
			this.closed = true;
			this.onClose();
		}
	}

	private feed(d: Buffer): void {
		this.buf = Buffer.concat([this.buf, d]);
		for (;;) {
			if (this.buf.length < 2) {
				return;
			}
			const opcode = this.buf[0] & 0x0f;
			const masked = (this.buf[1] & 0x80) !== 0;
			let len = this.buf[1] & 0x7f;
			let off = 2;
			if (len === 126) {
				if (this.buf.length < 4) {
					return;
				}
				len = this.buf.readUInt16BE(2);
				off = 4;
			} else if (len === 127) {
				if (this.buf.length < 10) {
					return;
				}
				len = Number(this.buf.readBigUInt64BE(2));
				off = 10;
			}
			const maskLen = masked ? 4 : 0;
			if (this.buf.length < off + maskLen + len) {
				return;
			}
			let payload = this.buf.subarray(off + maskLen, off + maskLen + len);
			if (masked) {
				const mask = this.buf.subarray(off, off + 4);
				payload = Buffer.from(payload.map((b, i) => b ^ mask[i % 4]));
			}
			this.buf = this.buf.subarray(off + maskLen + len);
			if (opcode === 0x1 || opcode === 0x2 || opcode === 0x0) {
				this.onMessage(Buffer.from(payload));
			} else if (opcode === 0x8) {
				this.close();
				return;
			} else if (opcode === 0x9) {
				this.sendFrame(0xa, payload);
			}
		}
	}

	private sendFrame(opcode: number, payload: Buffer): void {
		if (this.closed) {
			return;
		}
		const mask = crypto.randomBytes(4);
		const n = payload.length;
		const head = n < 126 ? Buffer.from([0x80 | opcode, 0x80 | n])
			: n < 65536 ? Buffer.from([0x80 | opcode, 0x80 | 126, n >> 8, n & 0xff])
				: (() => { const b = Buffer.alloc(10); b[0] = 0x80 | opcode; b[1] = 0x80 | 127; b.writeBigUInt64BE(BigInt(n), 2); return b; })();
		const body = Buffer.from(payload.map((b, i) => b ^ mask[i % 4]));
		this.socket.write(Buffer.concat([head, mask, body]));
	}

	send(data: Buffer): void {
		this.sendFrame(0x2, data);
	}

	close(): void {
		if (!this.closed) {
			this.sendFrame(0x8, Buffer.alloc(0));
			this.socket.end();
			this.finish();
		}
	}
}

function wsConnect(url: URL, headers: Record<string, string>): Promise<WsConnection> {
	return new Promise((resolve, reject) => {
		const key = crypto.randomBytes(16).toString('base64');
		const req = http.request(url, {
			headers: { ...headers, Connection: 'Upgrade', Upgrade: 'websocket', 'Sec-WebSocket-Version': '13', 'Sec-WebSocket-Key': key },
		});
		req.on('upgrade', (res, socket, head) => {
			const accept = crypto.createHash('sha1').update(key + '258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest('base64');
			if (res.headers['sec-websocket-accept'] !== accept) {
				socket.destroy();
				reject(new Error('the hub answered the WebSocket handshake wrongly'));
				return;
			}
			socket.setNoDelay(true);
			resolve(new WsConnection(socket, head));
		});
		req.on('response', (res) => {
			const chunks: Buffer[] = [];
			res.on('data', (c: Buffer) => chunks.push(c));
			res.on('end', () => reject(new HubError(`TCF relay refused (HTTP ${res.statusCode}): ${Buffer.concat(chunks).toString('utf8')}`, res.statusCode ?? 0)));
		});
		req.on('error', reject);
		req.end();
	});
}

/** A local TCP port whose connections go to a session's TCF server through the hub. */
export interface TcfRelay {
	port: number;
	close(): void;
}

export function startTcfRelay(client: HubClient, session: string, log: (l: string) => void): Promise<TcfRelay> {
	const url = new URL(`/api/v1/sessions/${encodeURIComponent(session)}/tcf`, client.opts.url);
	const headers: Record<string, string> = { 'X-Aurix-Client': client.opts.clientId };
	if (client.opts.token) {
		headers['Authorization'] = `Bearer ${client.opts.token}`;
	}
	const sockets = new Set<net.Socket>();
	const server = net.createServer((sock) => {
		sockets.add(sock);
		sock.setNoDelay(true);
		sock.pause();
		wsConnect(url, headers).then((ws) => {
			ws.onMessage = (d) => sock.write(d);
			ws.onClose = () => sock.end();
			sock.on('data', (d: Buffer) => ws.send(d));
			sock.on('close', () => { ws.close(); sockets.delete(sock); });
			sock.on('error', () => ws.close());
			sock.resume();
		}, (e) => {
			log(`TCF relay: ${e instanceof Error ? e.message : String(e)}`);
			sock.destroy();
		});
	});
	return new Promise((resolve, reject) => {
		server.once('error', reject);
		server.listen(0, '127.0.0.1', () => {
			const port = (server.address() as net.AddressInfo).port;
			log(`TCF relay for session ${session}: 127.0.0.1:${port} -> ${url.href}`);
			resolve({
				port,
				close: () => {
					for (const s of sockets) {
						s.destroy();
					}
					server.close();
				},
			});
		});
	});
}
