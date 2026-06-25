import { useCallback, useEffect, useState, type ReactNode } from "react";
import {
	Activity,
	ArrowRight,
	CheckCircle2,
	Database,
	Library,
	LockKeyhole,
	Maximize2,
	Minimize2,
	PanelRightOpen,
	RefreshCw,
	Search,
	ShieldCheck,
	SlidersHorizontal,
	Sparkles,
	type LucideIcon,
} from "lucide-react";
import logoImage from "../assets/images/logo.svg";
import { useLanguage, type Language } from "../components/LanguageProvider";
import LanguageSwitcher from "../components/layout/LanguageSwitcher";
import ThemeSwitcher from "../components/layout/ThemeSwitcher";
import { BROWSER_EXTENSION_LINKS } from "../lib/browser-extensions";
import { setDocumentMeta } from "../utils/seo";

type ContentSection = {
	id: string;
	nav: string;
	title: string;
	body: string;
	icon: LucideIcon;
	items: Array<{
		title: string;
		body: string;
	}>;
};

const conceptTranslations: Record<Language, Record<string, string>> = {
	en: {
		"Nav Why": "Why",
		"Nav Import": "Import",
		"Nav Skill": "Skill",
		"Nav Client": "Client",
		"Nav Control": "Control",
		"Nav Trust": "Trust",
	},
	ja: {},
	zh: {
		"Progressive MCP management": "渐进式 MCP 管理",
		"Download MCPMate": "下载 MCPMate",
		"Read quickstart": "阅读快速开始",
		"Get MCPMate": "获取 MCPMate",
		"Nav Why": "缘由",
		"Nav Import": "接入",
		"Nav Skill": "Skill",
		"Nav Client": "客户端",
		"Nav Control": "控制",
		"Nav Trust": "信任",
		"MAIN": "主要模块",
		"Advanced": "高级",
		"Inspect": "检查",
		"MCP gets messy before it looks like infrastructure.": "MCP 在成为基础设施之前，通常先变得混乱。",
		"MCPMate packages raw MCP capabilities into skill-shaped workflows, then helps you distribute, control, and inspect them from one local workspace.": "MCPMate 将原始 MCP 能力封装成面向技能的工作流，再从一个本地工作空间完成分发、控制和检查。",
		"You should not be stuck untangling MCP.": "你不该被困在 MCP 的这些麻烦里。",
		"MCPMate turns scattered sources, client differences, copied configs, noisy capabilities, missing workflow intent, and unclear runtime evidence into a manageable local workflow.": "MCPMate 将分散来源、客户端差异、复制配置、能力噪音、缺失的工作意图和不清晰的运行证据，整理回一条可管理的本地工作流。",
		"Discovery is scattered": "发现入口分散",
		"Servers appear in registries, GitHub pages, client configs, chat snippets, and docs.": "服务器会出现在注册表、GitHub 页面、客户端配置、聊天片段和文档里。",
		"Client setup varies": "客户端配置方式各不相同",
		"Claude, Cursor, Codex, VS Code, and custom clients expect different configuration paths.": "Claude、Cursor、Codex、VS Code 和自定义客户端需要不同的配置路径。",
		"Copied config drifts": "复制出来的配置会漂移",
		"Each manual edit creates another place where commands, env vars, and transports can fall out of sync.": "每一次手工编辑都会多出一个命令、环境变量和传输方式可能失同步的位置。",
		"Capability lists get noisy": "能力列表会迅速变吵",
		"Every client seeing every raw tool makes context larger and hides the actual workflow intent.": "每个客户端都看到所有原始工具，会放大上下文，也掩盖真实工作意图。",
		"Work intent is missing": "工作意图缺位",
		"A tool list says what exists, but not which capabilities belong together for research, coding, analysis, or operations.": "工具列表只能说明有什么，不能说明哪些能力应该为调研、编码、分析或运维组合在一起。",
		"Failures are hard to locate": "故障难以定位",
		"Without runtime evidence, users guess whether a failure belongs to the client, server, secret, or transport.": "缺少运行证据时，用户只能猜问题来自客户端、服务器、密钥还是传输层。",
		"Start from where raw MCP capability enters the workspace.": "从原始 MCP 能力进入工作空间的地方开始。",
		"MCPMate's first job is to shorten the path from discovery to a reviewed local capability source.": "MCPMate 首先要缩短从发现能力到形成可审查本地能力源的路径。",
		"Detect existing setup": "检测已有设置",
		"Onboarding can detect installed AI clients and review MCP server definitions already present in local client files.": "Onboarding 可以检测已安装的 AI 客户端，并检查本地客户端文件中已经存在的 MCP 服务器定义。",
		"Import from messy sources": "从混杂来源导入",
		"Use Market entries, browser extension handoff, pasted snippets, JSON, JSON5, TOML, bundles, or existing configs.": "可以使用 Market 条目、浏览器扩展转交、粘贴片段、JSON、JSON5、TOML、bundle 或已有配置。",
		"Preview before install": "安装前预览",
		"Inspect tools, prompts, resources, templates, capability counts, source context, and preview errors before saving.": "保存前先检查工具、提示词、资源、模板、能力数量、来源上下文和预览错误。",
		"Validate the import": "验证导入",
		"Dry-run validation flags duplicates and blocking issues before the real import button becomes available.": "Dry-run 验证会在真正导入前标出重复项和阻塞问题。",
		"Turn raw capabilities into skill-shaped work units.": "将原始能力转化为技能形态的工作单元。",
		"After import, MCPMate can group servers, capability summaries, runtime needs, and validation rules into reusable workflow packages.": "导入之后，MCPMate 可以把服务器、能力摘要、运行时需求和验证规则组织成可复用的工作流包。",
		"Server records": "服务器记录",
		"Commands, environment variables, transport type, instances, source metadata, and docs context live with the server.": "命令、环境变量、传输类型、实例、来源元数据和文档上下文都随服务器一起保存。",
		"Capability inventory": "能力清单",
		"Tools, prompts, resources, and templates are discovered and cached so users can inspect what each source exposes.": "工具、提示词、资源和模板会被发现并缓存，用户可以检查每个来源暴露了什么。",
		"Skill definition": "Skill 定义",
		"A skill can carry scenario intent, selected capabilities, runtime requirements, validation steps, and avoid rules.": "一个 skill 可以携带场景意图、选定能力、运行时需求、验证步骤和规避规则。",
		"One review path": "统一审查路径",
		"Market installs, extension captures, drag-and-drop, and paste imports converge on the same local review model.": "Market 安装、扩展捕获、拖放和粘贴导入都会汇合到同一套本地审查模型。",
		"Distribute packaged capability to the right client surface.": "把封装后的能力分发到合适的客户端表面。",
		"MCPMate turns reviewed capability into client-ready outputs, with the right level of runtime control and nearby rollback protection for each workflow.": "MCPMate 将审查后的能力转化为客户端可用输出，并为每类工作流保留恰当的运行时控制和近处回滚保护。",
		"Compatible app presets": "兼容应用预设",
		"Claude Desktop, Cursor, Codex, VS Code, Zed, and custom clients can start from known local configuration targets.": "Claude Desktop、Cursor、Codex、VS Code、Zed 和自定义客户端可以从已知本地配置目标开始。",
		"Verified write targets": "验证写入目标",
		"Client config writes only apply after MCPMate confirms the app has a local, writable configuration target.": "只有在 MCPMate 确认应用有本地可写配置目标之后，客户端配置写入才会执行。",
		"Transparent mode": "透明模式",
		"Write selected servers into the client's native config when compatibility and direct control matter most.": "当兼容性和直接控制最重要时，将选定服务器写入客户端原生配置。",
		"Hosted mode": "托管模式",
		"Keep MCPMate in the local runtime path when skill switching, visibility control, and inspection matter.": "当 skill 切换、可见性控制和检查能力重要时，让 MCPMate 留在本地运行路径中。",
		"Unify mode": "统一模式",
		"Start with a smaller built-in control surface when broad capability should be discovered during the session.": "当更广泛能力应该在会话中发现时，从一个更小的内置控制表面开始。",
		"Control which packaged capability each workflow can use.": "控制每个工作流可以使用哪些封装后的能力。",
		"Skill-shaped distribution is the core difference between a config editor and a managed MCP workflow.": "技能形态的分发，是配置编辑器与可管理 MCP 工作流之间的核心差异。",
		"Profiles as current policy": "配置集作为当前策略",
		"Profiles attach servers and filter tools, prompts, resources, and templates until skills become the clearer user-facing package.": "在 skills 成为更清晰的用户侧封装之前，配置集负责关联服务器并过滤工具、提示词、资源和模板。",
		"Client-specific sources": "按客户端选择来源",
		"A client can follow active profiles, selected shared profiles, or a custom profile without duplicating server setup.": "客户端可以跟随活动配置集、选定共享配置集或自定义配置集，而无需复制服务器设置。",
		"Less visible clutter": "减少可见噪音",
		"The client sees a focused capability package instead of every raw tool the local server library can expose.": "客户端看到的是聚焦的能力包，而不是本地服务器库能暴露的所有原始工具。",
		"Live exposure matters": "实时暴露关系很重要",
		"Capability edits affect merged runtime exposure, so MCPMate treats them as operational changes, not labels.": "能力编辑会影响合并后的运行时暴露面，因此 MCPMate 将其视为运行变更，而不是标签。",
		"Inspect the packaged workflow instead of guessing.": "检查封装后的工作流，而不是靠猜。",
		"MCPMate needs to prove it is more than config storage by showing what changed, what ran, and where to investigate next.": "MCPMate 需要通过展示发生了什么变化、实际运行了什么、接下来该查哪里，来证明它不只是配置存储。",
		"Readiness and bindings": "就绪状态与绑定关系",
		"See server status, runtime binding state, capability discovery, and cache health before blaming a client.": "在归咎客户端之前，先查看服务器状态、运行时绑定、能力发现和缓存健康度。",
		"Controlled Inspector calls": "受控 Inspector 调用",
		"Run direct calls against connected servers and compare proxy-path behavior during troubleshooting.": "排障时对已连接服务器执行直接调用，并比较代理路径行为。",
		"Operational logs": "运行日志",
		"Audit timelines track profile lifecycle, client apply actions, backups, restores, server imports, and toggles.": "审计时间线跟踪配置集生命周期、客户端应用动作、备份、恢复、服务器导入和开关。",
		"Maintenance loops": "维护闭环",
		"Reset runtime caches, refresh capability state, and rerun checks after heavy profile or server changes.": "在重度配置集或服务器变更后，重置运行时缓存、刷新能力状态并重新执行检查。",
		"The trust boundary should be specific.": "信任边界必须具体。",
		"MCPMate can be serious without pretending to be a cloud enterprise gateway.": "MCPMate 可以严肃可靠，但不假装自己是云端企业网关。",
		"Local-first custody": "本地优先的托管边界",
		"Configuration, runtime state, and operational evidence are centered on the user's machine.": "配置、运行时状态和运行证据都以用户机器为中心。",
		"Read-only discovery": "只读发现",
		"Public Discovery helps with starter data and catalogs without becoming a remote control plane.": "Public Discovery 提供起始数据和目录，但不成为远程控制平面。",
		"Secure credential paths": "安全凭据路径",
		"Secure Store and OAuth-enabled installs keep sensitive setup closer to runtime custody, while transparent export remains an explicit trade-off.": "Secure Store 和 OAuth 安装让敏感设置更靠近运行时托管；透明导出则是明确的取舍。",
		"Clear current boundary": "清晰的当前边界",
		"MCPMate is local-first and team-ready in posture; cloud tenant governance, centralized RBAC, and compliance certification are not current claims.": "MCPMate 当前是本地优先、可面向团队使用的姿态；云租户治理、集中式 RBAC 和合规认证不是当前承诺。",
		"Skills are the package. Distribution is the larger path.": "Skills 是能力封装，分发是更大的路径。",
		"The page can speak from the skills-over-MCP release, while still keeping the longer governance roadmap honest.": "页面可以从 skills over MCP 版本出发表达，同时诚实保留更长期的治理路线。",
		"Skills become the user-facing package": "Skills 成为用户可理解的能力包",
		"Profiles remain useful policy infrastructure, while skills can become the clearer way to name reusable workflow capability.": "配置集仍是有用的策略基础设施，而 skills 可以成为命名可复用工作流能力的更清晰方式。",
		"Distribution can become task-shaped": "分发可以变成任务形态",
		"A package can describe scenario, selected capabilities, ordered steps, validation rules, and avoid rules before reaching a client.": "一个能力包在到达客户端前，可以描述场景、选定能力、步骤顺序、验证规则和规避规则。",
		"Discovery can prefer skill-first entry": "发现入口可以优先面向 skill",
		"Users can start from what they need to do, then inspect the raw tools, prompts, resources, and templates behind it.": "用户可以从要完成的事情开始，再检查背后的原始工具、提示词、资源和模板。",
		"Request-scoped governance is later": "请求级治理属于后续阶段",
		"Client, project, role, actor, and operation metadata may eventually shape access decisions at the moment of use.": "客户端、项目、角色、操作者和操作元数据，未来可能在使用当下参与访问决策。",
		"Quick start": "快速开始",
		"Package one real MCP workflow.": "封装一个真实 MCP 工作流。",
		"Start with the path to first value: install MCPMate, import one real source, shape the capability into a workflow, then connect one AI client.": "从第一价值路径开始：安装 MCPMate，导入一个真实来源，把能力整理成工作流，然后连接一个 AI 客户端。",
		"Install": "安装",
		"Run MCPMate locally.": "在本地运行 MCPMate。",
		"Import": "导入",
		"Bring in one real MCP capability source.": "接入一个真实 MCP 能力来源。",
		"Package": "封装",
		"Shape it into a skill-ready workflow.": "整理成面向 skill 的工作流。",
		"Distribute": "分发",
		"Send it to one AI client and inspect what runs.": "发送到一个 AI 客户端，并检查实际运行内容。",
		"Product": "产品",
		"Capability layer": "能力层",
		"Resources": "资源",
		"Project": "项目",
		"Download": "下载",
		"Quickstart": "快速开始",
		"Browser extension": "浏览器扩展",
		"Client configuration": "客户端配置",
		"Market": "市场",
		"Profiles": "配置集",
		"Runtime": "运行时",
		"Client backups": "客户端备份",
		"Documentation": "文档",
		"Changelog": "更新日志",
		"Roadmap": "路线图",
		"Chrome extension": "Chrome 扩展",
		"Edge extension": "Edge 扩展",
		"GitHub": "GitHub",
		"Email": "邮箱",
		"Privacy": "隐私",
		"Terms": "条款",
		"Discord": "Discord",
		"© 2026 MCPMate": "© 2026 MCPMate",
		"Skills over MCP: package local server capability into inspectable workflows, then distribute it to the AI clients that should use it.": "Skills over MCP：把本地服务器能力封装成可检查的工作流，再分发给真正应该使用它的 AI 客户端。",
		"© {year} MCPMate. All rights reserved.": "© {year} MCPMate. 保留所有权利。",
		"MCPMate Progressive MCP Management Homepage Concept": "MCPMate 渐进式 MCP 管理首页概念",
		"A homepage concept for MCPMate's progressive MCP management narrative.": "面向 MCPMate 渐进式 MCP 管理叙事的首页概念。",
		"Board demo target": "Board demo 目标",
		"Embedded board-owned demo surface": "嵌入由 board 拥有的 demo 表面",
		"Loaded from a board-owned demo route with mock MCPMate data and no persisted writes.": "从 board 拥有的 demo 路由加载，使用 MCPMate mock 数据且不持久化写入。",
		"Loading board demo...": "正在加载 board demo...",
		"Checking board demo target...": "正在检查 board demo 目标...",
		"Board demo target is not reachable.": "Board demo 目标当前不可访问。",
		"Start the board demo server, then reload this frame.": "启动 board demo server 后，重新加载这个 frame。",
		"Open demo target": "打开 demo 目标",
		"Reload demo frame": "重新加载 demo frame",
		"onboarding": "onboarding",
		"overview": "overview",
		"profiles": "profiles",
		"clients": "clients",
		"servers": "servers",
		"market": "market",
		"runtime": "runtime",
		"api docs": "api docs",
		"security": "security",
		"Maximize demo window": "最大化 demo 窗口",
		"Restore demo window": "恢复 demo 窗口",
	},
};

function useConceptText() {
	const { language } = useLanguage();
	const translations = conceptTranslations[language] ?? conceptTranslations.en;

	return useCallback((value: string) => {
		const translated = translations[value] ?? conceptTranslations.en[value] ?? value;
		return translated.replace("{year}", new Date().getFullYear().toString());
	}, [translations]);
}

const quickStartSteps = [
	["Install", "Run MCPMate locally."],
	["Import", "Bring in one real MCP capability source."],
	["Package", "Shape it into a skill-ready workflow."],
	["Distribute", "Send it to one AI client and inspect what runs."],
];

const conceptNavLinks = [
	{ href: "#problem", label: "Nav Why" },
	{ href: "#adoption", label: "Nav Import" },
	{ href: "#workspace", label: "Nav Skill" },
	{ href: "#rollout", label: "Nav Client" },
	{ href: "#control", label: "Nav Control" },
	{ href: "#trust", label: "Nav Trust" },
];

const problemItems = [
	{
		title: "Discovery is scattered",
		body: "Servers appear in registries, GitHub pages, client configs, chat snippets, and docs.",
	},
	{
		title: "Client setup varies",
		body: "Claude, Cursor, Codex, VS Code, and custom clients expect different configuration paths.",
	},
	{
		title: "Copied config drifts",
		body: "Each manual edit creates another place where commands, env vars, and transports can fall out of sync.",
	},
	{
		title: "Capability lists get noisy",
		body: "Every client seeing every raw tool makes context larger and hides the actual workflow intent.",
	},
	{
		title: "Work intent is missing",
		body: "A tool list says what exists, but not which capabilities belong together for research, coding, analysis, or operations.",
	},
	{
		title: "Failures are hard to locate",
		body: "Without runtime evidence, users guess whether a failure belongs to the client, server, secret, or transport.",
	},
];

const contentSections: ContentSection[] = [
	{
		id: "adoption",
		nav: "Adopt",
		title: "Start from where raw MCP capability enters the workspace.",
		body: "MCPMate's first job is to shorten the path from discovery to a reviewed local capability source.",
		icon: Search,
		items: [
			{
				title: "Detect existing setup",
				body: "Onboarding can detect installed AI clients and review MCP server definitions already present in local client files.",
			},
			{
				title: "Import from messy sources",
				body: "Use Market entries, browser extension handoff, pasted snippets, JSON, JSON5, TOML, bundles, or existing configs.",
			},
			{
				title: "Preview before install",
				body: "Inspect tools, prompts, resources, templates, capability counts, source context, and preview errors before saving.",
			},
			{
				title: "Validate the import",
				body: "Dry-run validation flags duplicates and blocking issues before the real import button becomes available.",
			},
		],
	},
	{
		id: "workspace",
		nav: "Workspace",
		title: "Turn raw capabilities into skill-shaped work units.",
		body: "After import, MCPMate can group servers, capability summaries, runtime needs, and validation rules into reusable workflow packages.",
		icon: Library,
		items: [
			{
				title: "Server records",
				body: "Commands, environment variables, transport type, instances, source metadata, and docs context live with the server.",
			},
			{
				title: "Capability inventory",
				body: "Tools, prompts, resources, and templates are discovered and cached so users can inspect what each source exposes.",
			},
			{
				title: "Skill definition",
				body: "A skill can carry scenario intent, selected capabilities, runtime requirements, validation steps, and avoid rules.",
			},
			{
				title: "One review path",
				body: "Market installs, extension captures, drag-and-drop, and paste imports converge on the same local review model.",
			},
		],
	},
	{
		id: "rollout",
		nav: "Rollout",
		title: "Distribute packaged capability to the right client surface.",
		body: "MCPMate turns reviewed capability into client-ready outputs, with the right level of runtime control and nearby rollback protection for each workflow.",
		icon: PanelRightOpen,
		items: [
			{
				title: "Compatible app presets",
				body: "Claude Desktop, Cursor, Codex, VS Code, Zed, and custom clients can start from known local configuration targets.",
			},
			{
				title: "Verified write targets",
				body: "Client config writes only apply after MCPMate confirms the app has a local, writable configuration target.",
			},
			{
				title: "Transparent mode",
				body: "Write selected servers into the client's native config when compatibility and direct control matter most.",
			},
			{
				title: "Hosted mode",
				body: "Keep MCPMate in the local runtime path when skill switching, visibility control, and inspection matter.",
			},
			{
				title: "Unify mode",
				body: "Start with a smaller built-in control surface when broad capability should be discovered during the session.",
			},
		],
	},
	{
		id: "control",
		nav: "Control",
		title: "Control which packaged capability each workflow can use.",
		body: "Skill-shaped distribution is the core difference between a config editor and a managed MCP workflow.",
		icon: SlidersHorizontal,
		items: [
			{
				title: "Profiles as current policy",
				body: "Profiles attach servers and filter tools, prompts, resources, and templates until skills become the clearer user-facing package.",
			},
			{
				title: "Client-specific sources",
				body: "A client can follow active profiles, selected shared profiles, or a custom profile without duplicating server setup.",
			},
			{
				title: "Less visible clutter",
				body: "The client sees a focused capability package instead of every raw tool the local server library can expose.",
			},
			{
				title: "Live exposure matters",
				body: "Capability edits affect merged runtime exposure, so MCPMate treats them as operational changes, not labels.",
			},
		],
	},
	{
		id: "evidence",
		nav: "Evidence",
		title: "Inspect the packaged workflow instead of guessing.",
		body: "MCPMate needs to prove it is more than config storage by showing what changed, what ran, and where to investigate next.",
		icon: Activity,
		items: [
			{
				title: "Readiness and bindings",
				body: "See server status, runtime binding state, capability discovery, and cache health before blaming a client.",
			},
			{
				title: "Controlled Inspector calls",
				body: "Run direct calls against connected servers and compare proxy-path behavior during troubleshooting.",
			},
			{
				title: "Operational logs",
				body: "Audit timelines track profile lifecycle, client apply actions, backups, restores, server imports, and toggles.",
			},
			{
				title: "Maintenance loops",
				body: "Reset runtime caches, refresh capability state, and rerun checks after heavy profile or server changes.",
			},
		],
	},
];

const trustItems = [
	{
		title: "Local-first custody",
		body: "Configuration, runtime state, and operational evidence are centered on the user's machine.",
		icon: Database,
	},
	{
		title: "Read-only discovery",
		body: "Public Discovery helps with starter data and catalogs without becoming a remote control plane.",
		icon: ShieldCheck,
	},
	{
		title: "Secure credential paths",
		body: "Secure Store and OAuth-enabled installs keep sensitive setup closer to runtime custody, while transparent export remains an explicit trade-off.",
		icon: LockKeyhole,
	},
	{
		title: "Clear current boundary",
		body: "MCPMate is local-first and team-ready in posture; cloud tenant governance, centralized RBAC, and compliance certification are not current claims.",
		icon: CheckCircle2,
	},
];

const futureItems = [
	{
		title: "Skills become the user-facing package",
		body: "Profiles remain useful policy infrastructure, while skills can become the clearer way to name reusable workflow capability.",
	},
	{
		title: "Distribution can become task-shaped",
		body: "A package can describe scenario, selected capabilities, ordered steps, validation rules, and avoid rules before reaching a client.",
	},
	{
		title: "Discovery can prefer skill-first entry",
		body: "Users can start from what they need to do, then inspect the raw tools, prompts, resources, and templates behind it.",
	},
	{
		title: "Request-scoped governance is later",
		body: "Client, project, role, actor, and operation metadata may eventually shape access decisions at the moment of use.",
	},
];

type FooterLink = {
	label: string;
	href: string;
	external?: boolean;
};

const footerGroups: Array<{ title: string; links: FooterLink[] }> = [
	{
		title: "Product",
		links: [
			{ label: "Download", href: "/#download" },
			{ label: "Client configuration", href: "/docs/en/client-configuration" },
			{ label: "Profiles", href: "/docs/en/profiles" },
			{ label: "Runtime", href: "/docs/en/runtime" },
		],
	},
	{
		title: "Resources",
		links: [
			{ label: "Documentation", href: "/docs/en/quickstart" },
			{ label: "Changelog", href: "/docs/en/changelog" },
			{ label: "Roadmap", href: "/docs/en/roadmap" },
			...BROWSER_EXTENSION_LINKS.map((link) => ({
				label: link.id === "chrome" ? "Chrome extension" : "Edge extension",
				href: link.url,
				external: true,
			})),
		],
	},
	{
		title: "Project",
		links: [
			{ label: "GitHub", href: "https://github.com/loocor/mcpmate", external: true },
			{ label: "Email", href: "mailto:mcp@umate.ai" },
			{ label: "Privacy", href: "/privacy" },
			{ label: "Terms", href: "/terms" },
		],
	},
];

function cx(...parts: Array<string | false | null | undefined>) {
	return parts.filter(Boolean).join(" ");
}

function Section({
	children,
	className,
	id,
}: {
	children: ReactNode;
	className?: string;
	id?: string;
}) {
	return (
		<section id={id} className={cx("mx-auto max-w-7xl px-5 py-20 md:px-8 lg:py-28", className)}>
			{children}
		</section>
	);
}

function SectionHeader({
	title,
	body,
	invert = false,
}: {
	title: string;
	body: string;
	invert?: boolean;
}) {
	const text = useConceptText();

	return (
		<div className="max-w-3xl">
			<h2 className={cx("text-balance text-4xl font-semibold leading-tight md:text-6xl", invert ? "text-white" : "text-zinc-950")}>
				{text(title)}
			</h2>
			<p className={cx("mt-5 text-base leading-7 md:text-lg", invert ? "text-white/62" : "text-zinc-600")}>
				{text(body)}
			</p>
		</div>
	);
}

function BrandMark({ invert = false }: { invert?: boolean }) {
	const text = useConceptText();

	return (
		<div className="flex items-center gap-3">
			<span
				className={cx(
					"flex h-10 w-10 items-center justify-center rounded-md border",
					invert ? "border-white/20 bg-white" : "border-zinc-200 bg-white",
				)}
				aria-hidden
			>
				<img src={logoImage} alt="" className="h-7 w-7 object-contain dark:brightness-0 dark:invert" />
			</span>
			<div>
				<p className={cx("text-lg font-semibold leading-none", invert ? "text-white" : "text-zinc-950")}>MCPMate</p>
				<p className={cx("mt-1 text-xs", invert ? "text-white/45" : "text-zinc-500")}>
					{text("Progressive MCP management")}
				</p>
			</div>
		</div>
	);
}

function Actions({ invert = false }: { invert?: boolean }) {
	const text = useConceptText();

	return (
		<div className="flex flex-col gap-3 sm:flex-row">
			<a
				href="/#download"
				className={cx(
					"inline-flex min-h-12 items-center justify-center rounded-md px-5 text-sm font-semibold transition",
					invert ? "bg-white text-zinc-950 hover:bg-sky-50" : "bg-zinc-950 text-white hover:bg-sky-700",
				)}
			>
				{text("Download MCPMate")}
				<ArrowRight className="ml-2 h-4 w-4" aria-hidden />
			</a>
			<a
				href="/docs/en/quickstart"
				className={cx(
					"inline-flex min-h-12 items-center justify-center rounded-md border px-5 text-sm font-semibold transition",
					invert
						? "border-white/20 text-white hover:border-white/40 hover:bg-white/10"
						: "border-zinc-300 text-zinc-800 hover:border-zinc-500 hover:bg-zinc-100",
				)}
			>
				{text("Read quickstart")}
			</a>
		</div>
	);
}

const DEFAULT_BOARD_DEMO_URL = "http://127.0.0.1:5174/";
const BOARD_DEMO_TARGET_CHECK_TIMEOUT_MS = 2500;
const DEFAULT_BOARD_DEMO_ROUTE_KEY = "overview";

type BoardDemoFrameStatus = "checking" | "available" | "unavailable";
type BoardDemoRoute = {
	key: string;
	label: string;
	path: string;
};

const BOARD_DEMO_ROUTES: BoardDemoRoute[] = [
	{ key: "onboarding", label: "onboarding", path: "/onboarding" },
	{ key: "overview", label: "overview", path: "" },
	{ key: "profiles", label: "profiles", path: "/profiles" },
	{ key: "clients", label: "clients", path: "/clients" },
	{ key: "servers", label: "servers", path: "/servers" },
	{ key: "market", label: "market", path: "/market" },
	{ key: "runtime", label: "runtime", path: "/runtime" },
	{ key: "api-docs", label: "api docs", path: "/api-docs" },
	{ key: "security", label: "security", path: "/settings?tab=security" },
];

function resolveBoardDemoUrl(): string {
	const configuredUrl = import.meta.env.VITE_MCPMATE_BOARD_DEMO_URL;
	if (typeof configuredUrl === "string" && configuredUrl.trim().length > 0) {
		return configuredUrl.trim();
	}
	return DEFAULT_BOARD_DEMO_URL;
}

function buildBoardDemoRouteUrl(route: BoardDemoRoute): string {
	const base = resolveBoardDemoUrl().replace(/\/+$/, "");
	return route.path ? `${base}${route.path}` : `${base}/`;
}

function resolveBoardDemoRoute(routeKey: string): BoardDemoRoute {
	return (
		BOARD_DEMO_ROUTES.find((route) => route.key === routeKey) ??
		BOARD_DEMO_ROUTES.find((route) => route.key === DEFAULT_BOARD_DEMO_ROUTE_KEY) ??
		BOARD_DEMO_ROUTES[0]
	);
}

function useBodyScrollLock(locked: boolean): void {
	useEffect(() => {
		if (!locked) {
			return;
		}
		const previousOverflow = document.body.style.overflow;
		document.body.style.overflow = "hidden";
		return () => {
			document.body.style.overflow = previousOverflow;
		};
	}, [locked]);
}

function BoardDemoEmbed() {
	const [frameKey, setFrameKey] = useState(0);
	const [loaded, setLoaded] = useState(false);
	const [frameStatus, setFrameStatus] = useState<BoardDemoFrameStatus>("checking");
	const [selectedRouteKey, setSelectedRouteKey] = useState(DEFAULT_BOARD_DEMO_ROUTE_KEY);
	const [maximized, setMaximized] = useState(false);
	const text = useConceptText();
	const selectedRoute = resolveBoardDemoRoute(selectedRouteKey);
	const demoUrl = buildBoardDemoRouteUrl(selectedRoute);

	useBodyScrollLock(maximized);

	useEffect(() => {
		const controller = new AbortController();
		const timeoutId = window.setTimeout(() => {
			controller.abort();
		}, BOARD_DEMO_TARGET_CHECK_TIMEOUT_MS);
		let cancelled = false;

		setFrameStatus("checking");
		setLoaded(false);

		void fetch(demoUrl, {
			cache: "no-store",
			mode: "no-cors",
			signal: controller.signal,
		})
			.then(() => {
				if (!cancelled) {
					setFrameStatus("available");
				}
			})
			.catch(() => {
				if (!cancelled) {
					setFrameStatus("unavailable");
				}
			})
			.finally(() => {
				window.clearTimeout(timeoutId);
			});

		return () => {
			cancelled = true;
			window.clearTimeout(timeoutId);
			controller.abort();
		};
	}, [demoUrl, frameKey]);

	function reloadFrame(): void {
		setLoaded(false);
		setFrameStatus("checking");
		setFrameKey((current) => current + 1);
	}

	const maximizeLabel = text(maximized ? "Restore demo window" : "Maximize demo window");

	return (
		<>
			{maximized ? (
				<div
					className="fixed inset-0 z-[70] bg-slate-950/55 backdrop-blur-sm"
					aria-hidden="true"
				/>
			) : null}
			<div
				className={cx(
					"overflow-hidden border border-slate-200 bg-slate-950 shadow-[0_34px_110px_rgba(15,23,42,0.16)]",
					maximized
						? "fixed inset-3 z-[80] rounded-xl shadow-[0_40px_160px_rgba(0,0,0,0.48)] md:inset-6"
						: "rounded-xl",
				)}
			>
				<div className="flex min-h-12 flex-wrap items-center justify-between gap-3 border-b border-white/10 bg-slate-950 px-4 py-3 text-slate-100">
					<div className="flex min-w-0 items-center gap-3">
						<div className="flex shrink-0 items-center gap-2">
							<span className="h-3 w-3 rounded-full bg-[#ff5f57]" aria-hidden />
							<span className="h-3 w-3 rounded-full bg-[#febc2e]" aria-hidden />
							<button
								type="button"
								onClick={() => setMaximized((current) => !current)}
								className="flex h-3 w-3 items-center justify-center rounded-full bg-[#28c840] text-emerald-950"
								aria-label={maximizeLabel}
							>
								{maximized ? (
									<Minimize2 className="h-2 w-2 opacity-0 transition-opacity hover:opacity-80" aria-hidden />
								) : (
									<Maximize2 className="h-2 w-2 opacity-0 transition-opacity hover:opacity-80" aria-hidden />
								)}
							</button>
						</div>
						<div className="min-w-0">
							<p className="truncate text-sm font-semibold">MCPMate</p>
						</div>
					</div>
					<div className="flex min-w-0 flex-1 items-center justify-end">
						<button
							type="button"
							onClick={reloadFrame}
							className="flex h-8 w-8 shrink-0 items-center justify-center rounded-md text-slate-300 hover:bg-white/10 hover:text-white"
							aria-label={text("Reload demo frame")}
						>
							<RefreshCw className="h-4 w-4" aria-hidden />
						</button>
					</div>
				</div>
				<div
					className={cx(
						"relative bg-slate-100",
						maximized ? "h-[calc(100%-6.75rem)]" : "h-[34rem] md:h-[42.5rem]",
					)}
				>
					{frameStatus === "checking" ? (
						<div className="absolute inset-0 z-10 flex items-center justify-center bg-slate-100 text-sm font-medium text-slate-500">
							{text("Checking board demo target...")}
						</div>
					) : null}
					{frameStatus === "unavailable" ? (
						<div className="absolute inset-0 z-10 flex items-center justify-center bg-slate-100 p-6 text-center">
							<div className="max-w-xl">
								<p className="text-base font-semibold text-slate-800">
									{text("Board demo target is not reachable.")}
								</p>
								<p className="mt-2 text-sm leading-6 text-slate-500">
									{text("Start the board demo server, then reload this frame.")}
								</p>
								<code className="mt-4 block rounded-md border border-slate-200 bg-white px-3 py-2 text-xs text-slate-700">
									cd board && VITE_MCPMATE_BOARD_DEMO_MODE=1 bun run dev -- --host 127.0.0.1 --port 5174
								</code>
							</div>
						</div>
					) : null}
					{frameStatus === "available" ? (
						<>
							{!loaded ? (
								<div className="absolute inset-0 z-10 flex items-center justify-center bg-slate-100 text-sm font-medium text-slate-500">
									{text("Loading board demo...")}
								</div>
							) : null}
							<iframe
								key={frameKey}
								src={demoUrl}
								title="MCPMate board demo"
								className="h-full w-full bg-white"
								loading="eager"
								sandbox="allow-forms allow-same-origin allow-scripts"
								onLoad={() => setLoaded(true)}
							/>
						</>
					) : null}
				</div>
				<div className="border-t border-slate-200 bg-white px-4 py-3">
					<nav
						className="overflow-x-auto [scrollbar-width:none] [&::-webkit-scrollbar]:hidden"
						aria-label="MCPMate demo sections"
					>
						<div className="mx-auto flex w-max items-center justify-center whitespace-nowrap text-xs font-semibold text-slate-400">
							{BOARD_DEMO_ROUTES.map((route, index) => (
								<div key={route.key} className="flex items-center">
									{index > 0 ? (
										<span className="px-2 text-slate-300" aria-hidden>
											|
										</span>
									) : null}
									<button
										type="button"
										onClick={() => setSelectedRouteKey(route.key)}
										className={cx(
											"rounded-sm py-1 transition hover:text-slate-900",
											route.key === selectedRoute.key
												? "text-slate-950"
												: "text-slate-500",
										)}
									>
										{text(route.label)}
									</button>
								</div>
							))}
						</div>
					</nav>
				</div>
			</div>
		</>
	);
}

function HeroSection() {
	const text = useConceptText();

	return (
		<section className="overflow-hidden bg-white pt-24 text-zinc-950">
			<div className="relative mx-auto max-w-7xl px-5 pb-20 pt-8 md:px-8">
				<BoardDemoEmbed />
				<div className="mx-auto mt-14 max-w-4xl text-center">
					<h1 className="text-balance text-5xl font-semibold leading-[1.02] md:text-6xl xl:text-[4.6rem]">
						{text("MCP gets messy before it looks like infrastructure.")}
					</h1>
					<p className="mx-auto mt-6 max-w-2xl text-base leading-8 text-zinc-600 md:text-lg">
						{text("MCPMate packages raw MCP capabilities into skill-shaped workflows, then helps you distribute, control, and inspect them from one local workspace.")}
					</p>
					<div className="mt-8 flex justify-center">
						<Actions />
					</div>
				</div>
			</div>
		</section>
	);
}

function ProblemSection() {
	const text = useConceptText();

	return (
		<section id="problem" className="bg-zinc-50">
			<Section>
				<div className="grid gap-12 lg:grid-cols-[0.72fr_1.28fr]">
					<SectionHeader
						title="You should not be stuck untangling MCP."
						body="MCPMate turns scattered sources, client differences, copied configs, noisy capabilities, missing workflow intent, and unclear runtime evidence into a manageable local workflow."
					/>
					<div className="border border-zinc-200 bg-white">
						{problemItems.map((item) => (
							<div key={item.title} className="grid gap-3 border-b border-zinc-200 p-5 last:border-b-0 md:grid-cols-[13rem_1fr]">
								<h3 className="text-lg font-semibold text-zinc-950">{text(item.title)}</h3>
								<p className="text-sm leading-6 text-zinc-600">{text(item.body)}</p>
							</div>
						))}
					</div>
				</div>
			</Section>
		</section>
	);
}

function ContentBand({ section, index }: { section: ContentSection; index: number }) {
	const Icon = section.icon;
	const text = useConceptText();
	const backgroundClassName = index % 2 === 0 ? "bg-white" : "bg-zinc-50";

	return (
		<section id={section.id} className={backgroundClassName}>
			<Section>
				<div className="grid gap-12 lg:grid-cols-[0.72fr_1.28fr]">
					<div>
						<Icon className="h-8 w-8 text-sky-700" aria-hidden />
						<SectionHeader title={section.title} body={section.body} />
					</div>
					<div className="border border-zinc-200 bg-white">
						{section.items.map((item) => (
							<div key={item.title} className="grid gap-3 border-b border-zinc-200 p-5 last:border-b-0 md:grid-cols-[13rem_1fr]">
								<h3 className="text-lg font-semibold text-zinc-950">{text(item.title)}</h3>
								<p className="text-sm leading-6 text-zinc-600">{text(item.body)}</p>
							</div>
						))}
					</div>
				</div>
			</Section>
		</section>
	);
}

function TrustSection() {
	const text = useConceptText();

	return (
		<section id="trust" className="bg-zinc-50">
			<Section>
				<div className="grid gap-12 lg:grid-cols-[0.72fr_1.28fr]">
					<SectionHeader
						title="The trust boundary should be specific."
						body="MCPMate can be serious without pretending to be a cloud enterprise gateway."
					/>
					<div className="grid gap-4 md:grid-cols-2">
						{trustItems.map((item) => {
							const Icon = item.icon;

							return (
								<article key={item.title} className="border border-zinc-200 bg-white p-5">
									<Icon className="h-6 w-6 text-sky-700" aria-hidden />
									<h3 className="mt-6 text-xl font-semibold text-zinc-950">{text(item.title)}</h3>
									<p className="mt-3 text-sm leading-6 text-zinc-600">{text(item.body)}</p>
								</article>
							);
						})}
					</div>
				</div>
			</Section>
		</section>
	);
}

function FutureSection() {
	const text = useConceptText();

	return (
		<section id="future" className="bg-white">
			<Section>
				<div className="grid gap-12 lg:grid-cols-[0.72fr_1.28fr]">
					<div>
						<Sparkles className="h-8 w-8 text-sky-700" aria-hidden />
						<SectionHeader
							title="Skills are the package. Distribution is the larger path."
							body="The page can speak from the skills-over-MCP release, while still keeping the longer governance roadmap honest."
						/>
					</div>
					<div className="border border-zinc-200 bg-white">
						{futureItems.map((item, index) => (
							<div key={item.title} className="grid gap-4 border-b border-zinc-200 p-5 last:border-b-0 md:grid-cols-[4rem_1fr]">
								<span className="flex h-9 w-9 items-center justify-center rounded-md bg-sky-50 text-sm font-semibold text-sky-700 ring-1 ring-sky-100">
									{index + 1}
								</span>
								<div>
									<h3 className="text-xl font-semibold text-zinc-950">{text(item.title)}</h3>
									<p className="mt-2 text-sm leading-6 text-zinc-600">{text(item.body)}</p>
								</div>
							</div>
						))}
					</div>
				</div>
			</Section>
		</section>
	);
}

function FinalCtaSection() {
	const text = useConceptText();

	return (
		<section className="bg-zinc-50">
			<Section className="py-16 lg:py-20">
				<div className="grid gap-8 border border-zinc-200 bg-white p-6 md:p-8 lg:grid-cols-[0.85fr_1.15fr] lg:items-center">
					<div>
						<p className="text-sm font-semibold uppercase tracking-[0.14em] text-sky-700">{text("Quick start")}</p>
						<h2 className="mt-4 text-balance text-4xl font-semibold text-zinc-950 md:text-5xl">
							{text("Package one real MCP workflow.")}
						</h2>
						<p className="mt-4 max-w-2xl text-base leading-7 text-zinc-600">
							{text("Start with the path to first value: install MCPMate, import one real source, shape the capability into a workflow, then connect one AI client.")}
						</p>
						<div className="mt-7">
							<Actions />
						</div>
					</div>
					<div className="border border-zinc-200 bg-zinc-50">
						{quickStartSteps.map(([title, body], index) => (
							<div key={title} className="grid gap-4 border-b border-zinc-200 p-5 last:border-b-0 sm:grid-cols-[4rem_1fr]">
								<span className="flex h-9 w-9 items-center justify-center rounded-md bg-zinc-950 text-sm font-semibold text-white">
									{index + 1}
								</span>
								<div>
									<p className="text-xl font-semibold text-zinc-950">{text(title)}</p>
									<p className="mt-1 text-sm leading-6 text-zinc-600">{text(body)}</p>
								</div>
							</div>
						))}
					</div>
				</div>
			</Section>
		</section>
	);
}

function ConceptNav() {
	const text = useConceptText();

	return (
		<header className="concept-nav fixed inset-x-0 top-0 z-50 border-b px-5 py-3 backdrop-blur md:px-8">
			<div className="mx-auto flex max-w-7xl items-center justify-between gap-4">
				<a href="/" aria-label="MCPMate home" className="flex items-center gap-3">
					<span className="concept-nav__brand-icon flex h-10 w-10 items-center justify-center rounded-md border" aria-hidden>
						<img src={logoImage} alt="" className="h-7 w-7 object-contain" />
					</span>
					<span>
						<span className="concept-nav__brand-title block text-lg font-semibold leading-none">MCPMate</span>
						<span className="concept-nav__brand-subtitle mt-1 block text-xs">
							{text("Progressive MCP management")}
						</span>
					</span>
				</a>
				<div className="hidden items-center gap-5 lg:flex">
					<nav className="flex items-center gap-5 text-sm font-semibold">
						{conceptNavLinks.map((link) => (
							<a key={link.href} href={link.href} className="concept-nav__link">
								{text(link.label)}
							</a>
						))}
					</nav>
					<a href="/#download" className="concept-nav__cta rounded-md px-4 py-2 text-sm font-semibold">
						{text("Download MCPMate")}
					</a>
				</div>
			</div>
		</header>
	);
}

function ConceptFooter() {
	const text = useConceptText();

	return (
		<footer className="border-t border-zinc-200 bg-white">
			<div className="mx-auto grid max-w-7xl gap-10 px-5 py-12 md:px-8 lg:grid-cols-[1.2fr_1.8fr]">
				<div>
					<BrandMark />
					<p className="mt-5 max-w-md text-sm leading-6 text-zinc-600">
						{text("Skills over MCP: package local server capability into inspectable workflows, then distribute it to the AI clients that should use it.")}
					</p>
				</div>
				<div className="grid gap-8 sm:grid-cols-3">
					{footerGroups.map((group) => (
						<div key={group.title}>
							<h3 className="text-sm font-semibold text-zinc-950">{text(group.title)}</h3>
							<div className="mt-4 space-y-3">
								{group.links.map((link) => (
									<a
										key={link.label}
										href={link.href}
										target={link.external ? "_blank" : undefined}
										rel={link.external ? "noopener noreferrer" : undefined}
										className="flex items-center gap-1 text-sm text-zinc-500 hover:text-zinc-950"
									>
										{text(link.label)}
									</a>
								))}
							</div>
						</div>
					))}
				</div>
			</div>
			<div className="border-t border-zinc-200 px-5 py-5 text-sm text-zinc-500 md:px-8">
				<div className="mx-auto flex max-w-7xl flex-col gap-4 lg:flex-row lg:items-center lg:justify-between">
					<p>{text("© {year} MCPMate. All rights reserved.")}</p>
					<div className="flex items-center gap-3">
						<ThemeSwitcher />
						<LanguageSwitcher variant="footer" menuPlacement="above" />
					</div>
				</div>
			</div>
		</footer>
	);
}

export default function HomepageConcept() {
	const text = useConceptText();

	useEffect(() => {
		setDocumentMeta({
			title: text("MCPMate Progressive MCP Management Homepage Concept"),
			description: text("A homepage concept for MCPMate's progressive MCP management narrative."),
			pathname: "/concepts",
		});
		if (!window.location.hash) {
			window.scrollTo({ top: 0, left: 0, behavior: "instant" });
		}
	}, [text]);

	return (
		<div className="concept-page min-h-screen overflow-x-hidden bg-white text-zinc-950">
			<ConceptNav />
			<main>
				<HeroSection />
				<ProblemSection />
				{contentSections.map((section, index) => (
					<ContentBand key={section.id} section={section} index={index} />
				))}
				<TrustSection />
				<FutureSection />
				<FinalCtaSection />
			</main>
			<ConceptFooter />
		</div>
	);
}
