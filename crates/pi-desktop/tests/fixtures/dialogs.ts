// Test fixture for pi-desktop's headless UI tests (no model calls).
//   /dialogs  opens a confirm, an input, an editor, and a select dialog in
//             turn, then reports the answers through `notify`.
//   /timeout  opens a select that pi resolves itself after 1.5 s.
//   /status   sets a status entry and a widget.

interface Ui {
	confirm(title: string, message: string): Promise<boolean>;
	input(title: string, placeholder?: string): Promise<string | undefined>;
	editor(title: string, prefill?: string): Promise<string | undefined>;
	select(title: string, options: string[], opts?: { timeout?: number }): Promise<string | undefined>;
	notify(message: string, type?: "info" | "warning" | "error"): void;
	setStatus(key: string, text: string | undefined): void;
	setWidget(key: string, lines: string[] | undefined): void;
}

interface Pi {
	registerCommand(name: string, options: { description: string; handler(args: string, ctx: { ui: Ui }): Promise<void> }): void;
}

export default function (pi: Pi) {
	pi.registerCommand("dialogs", {
		description: "pi-desktop test: confirm, input, editor, select",
		handler: async (_args, ctx) => {
			const confirmed = await ctx.ui.confirm("确认标题", "继续吗？");
			const name = await ctx.ui.input("名字", "占位");
			const text = await ctx.ui.editor("编辑", "预填");
			const pick = await ctx.ui.select("选一个", ["甲", "乙", "丙"]);
			ctx.ui.notify(`dialogs confirmed=${confirmed} name=${name} pick=${pick} text=${text}`, "info");
		},
	});
	pi.registerCommand("timeout", {
		description: "pi-desktop test: a select that times out",
		handler: async (_args, ctx) => {
			const pick = await ctx.ui.select("很快超时", ["a", "b"], { timeout: 1500 });
			ctx.ui.notify(`timeout pick=${pick}`, "info");
		},
	});
	pi.registerCommand("status", {
		description: "pi-desktop test: status and widget",
		handler: async (_args, ctx) => {
			ctx.ui.setStatus("fixture", "fixture ok");
			ctx.ui.setWidget("fixture", ["widget line 1", "widget line 2"]);
		},
	});
}
