// pi-desktop accounts bridge. pi-desktop loads this file (`pi --mode rpc -e
// <file>`) into its own accounts process only; it is not a user extension.
//
// RPC mode has no login commands, so this extension drives pi's model
// runtime (the same `login`/`logout` the TUI's /login and /logout use) and
// reports back through `setWidget("pi-desktop/accounts", [json])`, one JSON
// event per record. Login prompts become `input`/`select` dialogs whose
// title is a JSON description of the prompt.
//
//   /pi-desktop-accounts list
//   /pi-desktop-accounts login <provider> <oauth|api_key>
//   /pi-desktop-accounts cancel
//   /pi-desktop-accounts logout <provider>
//
// The interfaces below declare only the parts of pi's API this file uses
// (pi loads extensions without type checking; see pi's docs/extensions.md).

const WIDGET = "pi-desktop/accounts";
const TIMEOUT_MS = 15_000;

type AuthType = "oauth" | "api_key";
type Emit = (event: Record<string, unknown>) => void;

interface AuthPrompt {
	type: "text" | "secret" | "select" | "manual_code";
	message?: string;
	placeholder?: string;
	options?: readonly { id: string; label: string }[];
	signal?: AbortSignal;
}

interface Provider {
	id: string;
	name?: string;
	auth?: {
		oauth?: { name?: string; loginLabel?: string; isSubscription?: boolean };
		apiKey?: { name?: string; login?: unknown };
	};
}

interface ModelRuntime {
	getProviders(): readonly Provider[];
	getProviderAuthStatus(id: string): { configured?: boolean; source?: string; label?: string } | undefined;
	listCredentials(options: { signal: AbortSignal }): Promise<readonly { providerId: string; type: AuthType }[]>;
	login(
		id: string,
		type: AuthType,
		interaction: { signal: AbortSignal; notify(event: Record<string, unknown>): void; prompt(prompt: AuthPrompt): Promise<string> },
	): Promise<unknown>;
	logout(id: string, options: { signal: AbortSignal }): Promise<void>;
}

interface Ui {
	setWidget(key: string, lines: string[]): void;
	input(title: string, placeholder: string | undefined, options: { signal: AbortSignal }): Promise<string | undefined>;
	select(title: string, options: string[], opts: { signal: AbortSignal }): Promise<string | undefined>;
}

interface CommandContext {
	ui: Ui;
	// `ModelRegistry` keeps the runtime in a private field; it is the only
	// path to login from an extension.
	modelRegistry?: { runtime?: Partial<ModelRuntime> };
}

interface Pi {
	registerCommand(name: string, options: { description: string; handler(args: string, ctx: CommandContext): Promise<void> }): void;
}

let current: AbortController | undefined;

export default function (pi: Pi) {
	pi.registerCommand("pi-desktop-accounts", {
		description: "pi-desktop accounts bridge (internal)",
		handler: async (args, ctx) => {
			const emit: Emit = (event) => ctx.ui.setWidget(WIDGET, [JSON.stringify(event)]);
			const [op = "", providerId = "", authType = ""] = args.trim().split(/\s+/);
			const runtime = ctx.modelRegistry?.runtime;
			if (typeof runtime?.login !== "function") {
				emit({ event: "error", op, message: "this pi version does not expose provider login to extensions" });
				return;
			}
			const full = runtime as ModelRuntime;
			try {
				switch (op) {
					case "list":
						emit({ event: "providers", ...(await list(full)) });
						break;
					case "login":
						await login(full, ctx.ui, emit, providerId, authType === "api_key" ? "api_key" : "oauth");
						break;
					case "cancel":
						current?.abort();
						break;
					case "logout":
						await full.logout(providerId, { signal: AbortSignal.timeout(TIMEOUT_MS) });
						emit({ event: "logged_out", providerId });
						break;
					default:
						emit({ event: "error", op, message: `unknown operation: ${op}` });
				}
			} catch (error) {
				emit({ event: "error", op, providerId, message: error instanceof Error ? error.message : String(error) });
			}
		},
	});
}

async function list(runtime: ModelRuntime) {
	const providers = runtime.getProviders().map((provider) => {
		const status = runtime.getProviderAuthStatus(provider.id) ?? {};
		const oauth = provider.auth?.oauth;
		const apiKey = provider.auth?.apiKey;
		return {
			id: provider.id,
			name: provider.name ?? provider.id,
			oauth: oauth ? { name: oauth.name ?? null, label: oauth.loginLabel ?? null, subscription: oauth.isSubscription === true } : null,
			apiKey: apiKey ? { name: apiKey.name ?? null, interactive: typeof apiKey.login === "function" } : null,
			configured: status.configured === true,
			source: status.label ?? status.source ?? null,
		};
	});
	const credentials = await runtime.listCredentials({ signal: AbortSignal.timeout(TIMEOUT_MS) });
	return { providers, credentials };
}

async function login(runtime: ModelRuntime, ui: Ui, emit: Emit, providerId: string, authType: AuthType) {
	current?.abort();
	const controller = new AbortController();
	current = controller;
	emit({ event: "login_started", providerId, authType });
	try {
		await runtime.login(providerId, authType, {
			signal: controller.signal,
			notify: (event) => emit({ ...event, event: "auth" }),
			prompt: (prompt) => ask(ui, emit, prompt, controller.signal),
		});
		emit({ event: "login_done", providerId, authType });
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		emit({ event: "login_failed", providerId, authType, cancelled: controller.signal.aborted || message === "Login cancelled", message });
	} finally {
		if (current === controller) current = undefined;
	}
}

async function ask(ui: Ui, emit: Emit, prompt: AuthPrompt, flow: AbortSignal): Promise<string> {
	const signal = prompt.signal ? AbortSignal.any([flow, prompt.signal]) : flow;
	const title = JSON.stringify({ type: prompt.type, message: prompt.message ?? "", placeholder: prompt.placeholder ?? null });
	let value: string | undefined;
	if (prompt.type === "select") {
		const options = prompt.options ?? [];
		const picked = await ui.select(title, options.map((option) => option.label), { signal });
		value = options.find((option) => option.label === picked)?.id;
	} else {
		value = await ui.input(title, prompt.placeholder, { signal });
	}
	// pi answered the dialog itself (the browser callback won, or the login
	// was cancelled); the client closes it.
	if (signal.aborted) emit({ event: "prompt_closed" });
	if (value === undefined) throw new Error("Login cancelled");
	return value;
}
