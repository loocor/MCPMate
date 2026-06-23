import { useCallback, useEffect, useState, type PointerEvent, type ReactNode } from "react";
import {
	Activity,
	AppWindow,
	ArrowRight,
	Bell,
	BookText,
	BookOpen,
	Bug,
	CheckCircle2,
	CircleUserRound,
	Database,
	FileSearch,
	KeyRound,
	Library,
	LayoutDashboard,
	LockKeyhole,
	Menu,
	MessageSquare,
	Moon,
	PanelRightOpen,
	RefreshCw,
	Search,
	Server,
	Settings,
	ShieldCheck,
	SlidersHorizontal,
	Sparkles,
	Square,
	Store,
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
		"Dashboard": "控制台",
		"Skills over MCP overview": "Skills over MCP 总览",
		"A local operating surface for packaging raw MCP capabilities into reviewable, reusable skill-shaped work units.": "一个本地运行表面，用来把原始 MCP 能力封装成可审查、可复用、面向 skill 的工作单元。",
		"Running": "运行中",
		"Core": "核心",
		"healthy": "健康",
		"Skills": "Skills",
		"8 ready": "8 个就绪",
		"Clients": "客户端",
		"4 bound": "4 个已绑定",
		"Capability source": "能力来源",
		"tools, prompts, resources, templates": "工具、提示词、资源、模板",
		"Skill layer": "Skill 层",
		"intent, selected capability, validation, avoid rules": "意图、选定能力、验证、规避规则",
		"Distribution": "分发",
		"profiles and client modes decide what each app receives": "配置集和客户端模式决定每个应用接收什么",
		"Capability discovery queue": "能力发现队列",
		"Market and browser handoff move scattered MCP sources into one review path before anything is installed.": "Market 和浏览器转交会在安装前把分散的 MCP 来源带入同一条审查路径。",
		"Reviewing": "审查中",
		"Drafts": "草稿",
		"4 queued": "4 个排队中",
		"Sources": "来源",
		"3 types": "3 类",
		"Checks": "检查",
		"ready": "就绪",
		"Browser handoff": "浏览器转交",
		"source page, snippet, and detected format retained": "保留来源页面、片段和检测到的格式",
		"Market record": "Market 记录",
		"server metadata and docs context stay attached": "服务器元数据和文档上下文保持附着",
		"Import review": "导入审查",
		"duplicates and blockers checked before save": "保存前检查重复项和阻塞项",
		"Servers": "服务器",
		"Reviewed server library": "已审查服务器库",
		"Servers remain the source layer: MCPMate imports them, previews their capabilities, and keeps origin context before packaging.": "服务器仍然是来源层：MCPMate 导入它们、预览能力，并在封装前保留来源上下文。",
		"Import ready": "可导入",
		"Ready": "就绪",
		"Desktop": "桌面端",
		"MCPMate Desktop is managing the local core and will stop it when the app quits.": "MCPMate 桌面端正在管理本地核心，并会在应用退出时停止它。",
		"v0.1.0": "v0.1.0",
		"Blocked": "阻塞",
		"Browser extension handoff": "浏览器扩展转交",
		"source page and metadata preserved": "保留来源页面和元数据",
		"Capability preview": "能力预览",
		"Dry-run validation": "Dry-run 验证",
		"duplicates and blockers checked first": "先检查重复项和阻塞项",
		"Client distribution control": "客户端分发控制",
		"MCPMate distributes packaged capability through the right client mode instead of copying every server into every app.": "MCPMate 通过合适的客户端模式分发封装后的能力，而不是把每个服务器复制到每个应用里。",
		"Targets": "目标",
		"6 known": "6 个已知",
		"Writable": "可写",
		"Mode": "模式",
		"Hosted": "Hosted",
		"Transparent": "Transparent",
		"write selected servers to native config": "将选定服务器写入原生配置",
		"keep MCPMate in the local runtime path": "让 MCPMate 留在本地运行路径中",
		"Unify": "Unify",
		"start from a smaller control surface": "从更小的控制表面开始",
		"Progressive skill policy": "渐进式 skill 策略",
		"Profiles are the current policy bridge between skill-shaped intent and the concrete MCP capabilities a client can see.": "配置集是当前连接 skill 形态意图与客户端可见具体 MCP 能力的策略桥梁。",
		"Exposure scoped": "暴露面已限定",
		"42 -> 11": "42 -> 11",
		"8 -> 3": "8 -> 3",
		"2 bound": "2 个已绑定",
		"Operating policy": "运行策略",
		"servers and capabilities grouped by workflow": "按工作流组织服务器和能力",
		"Client source": "客户端来源",
		"active, shared, or custom profile": "活动、共享或自定义配置集",
		"Live exposure": "实时暴露面",
		"runtime view changes with profile edits": "运行时视图随配置集编辑变化",
		"Evidence before guessing": "先看证据，再判断",
		"Runtime status, bindings, Inspector calls, logs, and backups show what changed and where to investigate next.": "运行时状态、绑定、Inspector 调用、日志和备份会显示变化内容和下一步调查位置。",
		"Verified": "已验证",
		"visible": "可见",
		"Inspector": "Inspector",
		"passed": "通过",
		"Audit": "审计",
		"recorded": "已记录",
		"Readiness": "就绪状态",
		"server status and cache health are visible": "服务器状态和缓存健康度可见",
		"Inspector calls": "Inspector 调用",
		"direct and proxy-path behavior compared": "对比直接路径和代理路径行为",
		"Maintenance": "维护",
		"refresh checks after heavy workflow changes": "重度工作流变更后刷新检查",
		"Settings": "设置",
		"Operator preferences": "操作者偏好",
		"Language, theme, default client mode, backup policy, and advanced visibility belong in the product shell, not in the marketing chrome.": "语言、主题、默认客户端模式、备份策略和高级可见性应该属于产品外壳，而不是营销页面装饰。",
		"Saved locally": "已本地保存",
		"Language": "语言",
		"English": "English",
		"Theme": "主题",
		"System": "跟随系统",
		"Language and theme are app-level preferences because they change the Board experience itself.": "语言和主题是应用级偏好，因为它们会改变 Board 本身的使用体验。",
		"Local setting": "本地设置",
		"Default client mode": "默认客户端模式",
		"Backup policy": "备份策略",
		"Before apply": "应用前",
		"Advanced nav": "高级导航",
		"Opt-in": "按需开启",
		"English, Chinese, and Japanese are first-class Board settings": "英文、中文和日文都是 Board 的一等设置",
		"system, light, and dark are explicit operator choices": "跟随系统、浅色、深色都是明确的操作者选择",
		"API Docs and debug surfaces stay opt-in": "API 文档和调试表面保持按需开启",
		"About": "关于",
		"About MCPMate": "关于 MCPMate",
		"Product identity, version channel, update notes, license posture, and local operating boundary belong inside the app too.": "产品身份、版本通道、更新说明、许可姿态和本地运行边界也应该在应用内呈现。",
		"Skills release": "Skills 版本",
		"Version": "版本",
		"Skills channel": "Skills 通道",
		"License": "许可",
		"Platform": "平台",
		"macOS · Windows · Linux": "macOS · Windows · Linux",
		"Latest focus": "最近重点",
		"skills over MCP, browser handoff, runtime evidence": "skills over MCP、浏览器转交、运行证据",
		"Boundary": "边界",
		"local-first and team-ready, not a cloud gateway claim": "本地优先且具备团队化姿态，但不是云网关承诺",
		"Next app update": "下一次应用更新",
		"make About useful for release notes and proof": "让关于页承载版本说明和证明材料",
		"The About surface carries release context, license posture, platform support, and the current operating boundary.": "关于界面承载发布上下文、许可姿态、平台支持和当前运行边界。",
		"Platforms": "平台",
		"macOS, Windows, Linux": "macOS、Windows、Linux",
		"Package capability as skill-shaped workflows.": "把能力封装成 skill 形态的工作流。",
		"Preserve browser and Market import evidence.": "保留浏览器和 Market 导入证据。",
		"Keep enterprise gateway claims out of the current boundary.": "避免在当前边界内宣称企业网关能力。",
		"Logs": "日志",
		"Operational evidence timeline": "运行证据时间线",
		"Logs keep profile changes, client apply actions, imports, backups, restores, and runtime checks close to the workflow.": "日志将配置集变更、客户端应用、导入、备份、恢复和运行检查保留在工作流附近。",
		"Events indexed": "事件已索引",
		"Profile events": "配置集事件",
		"Client applies": "客户端应用",
		"Restores": "恢复",
		"Change source": "变更来源",
		"profile edits, imports, and client writes are separated": "配置集编辑、导入和客户端写入被区分记录",
		"Evidence trail": "证据链",
		"who changed what, when, and which target was touched": "谁在何时改了什么、触及了哪个目标",
		"Recovery clue": "恢复线索",
		"backup and restore actions stay next to the risky operation": "备份和恢复动作紧邻风险操作",
		"Secrets": "密钥",
		"Credential custody surface": "凭据托管表面",
		"Secrets keeps sensitive runtime inputs managed locally instead of spreading raw values through copied client configs.": "Secrets 在本地管理敏感运行输入，避免原始值散落在复制出来的客户端配置中。",
		"Protected": "已保护",
		"Linked fields": "关联字段",
		"Exports": "导出",
		"explicit": "显式",
		"Local custody": "本地托管",
		"sensitive values stay closer to the runtime path": "敏感值更靠近运行路径",
		"Install flow": "安装流程",
		"server setup can reference stored secrets without pasting values everywhere": "服务器设置可引用已存密钥，而不是到处粘贴明文",
		"Trade-off": "取舍",
		"transparent export remains deliberate when native config needs it": "当原生配置需要时，透明导出仍是明确选择",
		"Account": "账户",
		"Local session boundary": "本地会话边界",
		"Account state stays visually separate from capability packaging so users can distinguish product session from MCP runtime control.": "账户状态与能力封装保持视觉分离，让用户区分产品会话和 MCP 运行时控制。",
		"Local session": "本地会话",
		"Plan": "计划",
		"workspace": "工作空间",
		"Sync": "同步",
		"off": "关闭",
		"Owner": "所有者",
		"local": "本地",
		"Identity": "身份",
		"shown as shell state, not capability policy": "作为外壳状态呈现，而不是能力策略",
		"Future": "未来",
		"team or cloud controls would need their own explicit surface": "团队或云端控制需要独立明确的表面",
		"Local Core": "本地核心",
		"Service": "服务",
		"running": "运行中",
		"The local MCPMate core is available for dashboard, API, and client runtime.": "本地 MCPMate 核心可供控制台、API 和客户端运行时使用。",
		"Refresh core status": "刷新核心状态",
		"Stop local core": "停止本地核心",
		"Start local core": "启动本地核心",
		"System Status": "系统状态",
		"Status": "状态",
		"Uptime": "运行时长",
		"2h 14m": "2 小时 14 分钟",
		"Total Profiles": "配置集总数",
		"Active Profiles": "活动配置集",
		"Visible tools": "可见工具",
		"Total Clients": "客户端总数",
		"Approved": "已批准",
		"Total Servers": "服务器总数",
		"Connected": "已连接",
		"Imports": "导入",
		"Metrics": "指标",
		"MCPMate process CPU and memory utilization sampled every 30 seconds": "每 30 秒采样 MCPMate 进程 CPU 和内存使用率",
		"Capability exposure": "能力暴露面",
		"Profile filtering keeps client context smaller without deleting source capability.": "配置集过滤让客户端上下文更小，同时不删除来源能力。",
		"Research profile": "调研配置集",
		"Coding profile": "编码配置集",
		"Ops profile": "运维配置集",
		"11 visible tools": "11 个可见工具",
		"16 visible tools": "16 个可见工具",
		"9 visible tools": "9 个可见工具",
		"Collapse sidebar": "收起侧边栏",
		"Token Savings": "Token 节省",
		"Estimated context savings from profile filtering": "基于配置集过滤估算的上下文节省",
		"API Docs": "API 文档",
		"Local API reference": "本地 API 参考",
		"API Docs stays in Advanced because it is useful for automation and debugging, not required for everyday MCP workflow setup.": "API 文档保留在高级区，因为它服务自动化和调试，而不是日常 MCP 工作流配置的必需入口。",
		"Endpoints": "端点",
		"Schema": "Schema",
		"Access": "访问",
		"available": "可用",
		"developer": "开发者",
		"Automation": "自动化",
		"scripts can use the same local operating state": "脚本可以使用同一份本地运行状态",
		"Debugging": "调试",
		"inspect request and response shape when the UI is not enough": "当 UI 不足以定位问题时检查请求和响应结构",
		"local API access is separate from cloud control-plane claims": "本地 API 访问与云控制平面承诺是两回事",
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
		"Common portals": "常见入口",
		"Chrome extension": "Chrome 扩展",
		"Edge extension": "Edge 扩展",
		"GitHub": "GitHub",
		"Email": "邮箱",
		"Privacy": "隐私",
		"Terms": "条款",
		"Skills over MCP: package local server capability into inspectable workflows, then distribute it to the AI clients that should use it.": "Skills over MCP：把本地服务器能力封装成可检查的工作流，再分发给真正应该使用它的 AI 客户端。",
		"© {year} MCPMate. All rights reserved.": "© {year} MCPMate. 保留所有权利。",
		"MCPMate Progressive MCP Management Homepage Concept": "MCPMate 渐进式 MCP 管理首页概念",
		"A homepage concept for MCPMate's progressive MCP management narrative.": "面向 MCPMate 渐进式 MCP 管理叙事的首页概念。",
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

type DesktopModuleKey =
	| "dashboard"
	| "profiles"
	| "clients"
	| "servers"
	| "market"
	| "audit"
	| "secrets"
	| "runtime"
	| "apiDocs"
	| "account"
	| "settings"
	| "about";

const desktopMainModuleOrder: DesktopModuleKey[] = ["dashboard", "profiles", "clients", "servers", "market"];
const desktopAdvancedModuleOrder: DesktopModuleKey[] = ["audit", "secrets", "runtime", "apiDocs"];
const desktopFooterModuleOrder: DesktopModuleKey[] = ["account", "settings"];

type DesktopSidebarModuleButtonProps = {
	moduleKey: DesktopModuleKey;
	activeModule: DesktopModuleKey;
	onModuleChange: (moduleKey: DesktopModuleKey) => void;
};

type DesktopSidebarModuleGroupProps = {
	label?: string;
	moduleKeys: DesktopModuleKey[];
	activeModule: DesktopModuleKey;
	onModuleChange: (moduleKey: DesktopModuleKey) => void;
};

const desktopModules: Record<
	DesktopModuleKey,
	{
		label: string;
		title: string;
		body: string;
		icon: LucideIcon;
		status: string;
		stats: Array<[string, string]>;
		rows: Array<[string, string]>;
	}
> = {
	dashboard: {
		label: "Dashboard",
		title: "Skills over MCP overview",
		body: "A local operating surface for packaging raw MCP capabilities into reviewable, reusable skill-shaped work units.",
		icon: LayoutDashboard,
		status: "Running",
		stats: [
			["Core", "healthy"],
			["Skills", "8 ready"],
			["Clients", "4 bound"],
		],
		rows: [
			["Capability source", "tools, prompts, resources, templates"],
			["Skill layer", "intent, selected capability, validation, avoid rules"],
			["Distribution", "profiles and client modes decide what each app receives"],
		],
	},
	market: {
		label: "Market",
		title: "Capability discovery queue",
		body: "Market and browser handoff move scattered MCP sources into one review path before anything is installed.",
		icon: Store,
		status: "Reviewing",
		stats: [
			["Drafts", "4 queued"],
			["Sources", "3 types"],
			["Checks", "ready"],
		],
		rows: [
			["Browser handoff", "source page, snippet, and detected format retained"],
			["Market record", "server metadata and docs context stay attached"],
			["Import review", "duplicates and blockers checked before save"],
		],
	},
	servers: {
		label: "Servers",
		title: "Reviewed server library",
		body: "Servers remain the source layer: MCPMate imports them, previews their capabilities, and keeps origin context before packaging.",
		icon: Server,
		status: "Import ready",
		stats: [
			["Drafts", "4"],
			["Ready", "12"],
			["Blocked", "1"],
		],
		rows: [
			["Browser extension handoff", "source page and metadata preserved"],
			["Capability preview", "tools, prompts, resources, templates"],
			["Dry-run validation", "duplicates and blockers checked first"],
		],
	},
	clients: {
		label: "Clients",
		title: "Client distribution control",
		body: "MCPMate distributes packaged capability through the right client mode instead of copying every server into every app.",
		icon: AppWindow,
		status: "Backup prepared",
		stats: [
			["Targets", "6 known"],
			["Writable", "4"],
			["Mode", "Hosted"],
		],
		rows: [
			["Transparent", "write selected servers to native config"],
			["Hosted", "keep MCPMate in the local runtime path"],
			["Unify", "start from a smaller control surface"],
		],
	},
	profiles: {
		label: "Profiles",
		title: "Progressive skill policy",
		body: "Profiles are the current policy bridge between skill-shaped intent and the concrete MCP capabilities a client can see.",
		icon: SlidersHorizontal,
		status: "Exposure scoped",
		stats: [
			["Tools", "42 -> 11"],
			["Prompts", "8 -> 3"],
			["Clients", "2 bound"],
		],
		rows: [
			["Operating policy", "servers and capabilities grouped by workflow"],
			["Client source", "active, shared, or custom profile"],
			["Live exposure", "runtime view changes with profile edits"],
		],
	},
	audit: {
		label: "Logs",
		title: "Operational evidence timeline",
		body: "Logs keep profile changes, client apply actions, imports, backups, restores, and runtime checks close to the workflow.",
		icon: FileSearch,
		status: "Events indexed",
		stats: [
			["Profile events", "18"],
			["Client applies", "7"],
			["Restores", "1"],
		],
		rows: [
			["Change source", "profile edits, imports, and client writes are separated"],
			["Evidence trail", "who changed what, when, and which target was touched"],
			["Recovery clue", "backup and restore actions stay next to the risky operation"],
		],
	},
	secrets: {
		label: "Secrets",
		title: "Credential custody surface",
		body: "Secrets keeps sensitive runtime inputs managed locally instead of spreading raw values through copied client configs.",
		icon: KeyRound,
		status: "Protected",
		stats: [
			["Secrets", "9"],
			["Linked fields", "14"],
			["Exports", "explicit"],
		],
		rows: [
			["Local custody", "sensitive values stay closer to the runtime path"],
			["Install flow", "server setup can reference stored secrets without pasting values everywhere"],
			["Trade-off", "transparent export remains deliberate when native config needs it"],
		],
	},
	runtime: {
		label: "Runtime",
		title: "Evidence before guessing",
		body: "Runtime status, bindings, Inspector calls, logs, and backups show what changed and where to investigate next.",
		icon: Activity,
		status: "Verified",
		stats: [
			["Bindings", "visible"],
			["Inspector", "passed"],
			["Audit", "recorded"],
		],
		rows: [
			["Readiness", "server status and cache health are visible"],
			["Inspector calls", "direct and proxy-path behavior compared"],
			["Maintenance", "refresh checks after heavy workflow changes"],
		],
	},
	apiDocs: {
		label: "API Docs",
		title: "Local API reference",
		body: "API Docs stays in Advanced because it is useful for automation and debugging, not required for everyday MCP workflow setup.",
		icon: Bug,
		status: "Opt-in",
		stats: [
			["Endpoints", "local"],
			["Schema", "available"],
			["Access", "developer"],
		],
		rows: [
			["Automation", "scripts can use the same local operating state"],
			["Debugging", "inspect request and response shape when the UI is not enough"],
			["Boundary", "local API access is separate from cloud control-plane claims"],
		],
	},
	account: {
		label: "Account",
		title: "Local session boundary",
		body: "Account state stays visually separate from capability packaging so users can distinguish product session from MCP runtime control.",
		icon: CircleUserRound,
		status: "Local session",
		stats: [
			["Plan", "workspace"],
			["Sync", "off"],
			["Owner", "local"],
		],
		rows: [
			["Identity", "shown as shell state, not capability policy"],
			["Boundary", "local-first operation remains the current product claim"],
			["Future", "team or cloud controls would need their own explicit surface"],
		],
	},
	settings: {
		label: "Settings",
		title: "Operator preferences",
		body: "Language, theme, default client mode, backup policy, and advanced visibility belong in the product shell, not in the marketing chrome.",
		icon: Settings,
		status: "Saved locally",
		stats: [
			["Language", "English"],
			["Theme", "System"],
			["Mode", "Hosted"],
		],
		rows: [
			["Language", "English, Chinese, and Japanese are first-class Board settings"],
			["Theme", "system, light, and dark are explicit operator choices"],
			["Advanced navigation", "API Docs and debug surfaces stay opt-in"],
		],
	},
	about: {
		label: "About",
		title: "About MCPMate",
		body: "Product identity, version channel, update notes, license posture, and local operating boundary belong inside the app too.",
		icon: BookText,
		status: "Skills release",
		stats: [
			["Version", "Skills channel"],
			["License", "AGPL-3.0"],
			["Platform", "macOS · Windows · Linux"],
		],
		rows: [
			["Latest focus", "skills over MCP, browser handoff, runtime evidence"],
			["Boundary", "local-first and team-ready, not a cloud gateway claim"],
			["Next app update", "make About useful for release notes and proof"],
		],
	},
};

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
			<p className={cx("mt-5 text-base leading-7 md:text-lg", invert ? "text-white/62" : "text-zinc-600")}>{text(body)}</p>
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
				<p className={cx("mt-1 text-xs", invert ? "text-white/45" : "text-zinc-500")}>{text("Progressive MCP management")}</p>
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
						<span className="concept-nav__brand-subtitle mt-1 block text-xs">{text("Progressive MCP management")}</span>
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

function BoardHeaderIconButton({
	icon: Icon,
	label,
}: {
	icon: LucideIcon;
	label: string;
}) {
	return (
		<button
			type="button"
			className="flex h-9 w-9 items-center justify-center rounded-md text-slate-500 transition hover:bg-slate-100 hover:text-slate-950"
			aria-label={label}
			title={label}
		>
			<Icon className="h-4 w-4" aria-hidden />
		</button>
	);
}

function SettingChoiceGroup({
	label,
	options,
	value,
	onChange,
}: {
	label: string;
	options: string[];
	value: string;
	onChange: (value: string) => void;
}) {
	const text = useConceptText();

	return (
		<div className="rounded-lg border border-slate-200 bg-white p-4">
			<p className="text-sm font-medium text-slate-500">{text(label)}</p>
			<div className="mt-3 grid gap-2 sm:grid-cols-3">
				{options.map((option) => {
					const active = option === value;

					return (
						<button
							key={option}
							type="button"
							onClick={() => onChange(option)}
							className={cx(
								"min-h-10 rounded-md border px-3 text-sm font-medium transition",
								active
									? "border-slate-950 bg-slate-950 text-white"
									: "border-slate-200 bg-white text-slate-500 hover:border-slate-300 hover:text-slate-950",
							)}
							aria-pressed={active}
						>
							{text(option)}
						</button>
					);
				})}
			</div>
		</div>
	);
}

function SettingsPreviewSurface({ className }: { className?: string }) {
	const [language, setLanguage] = useState("English");
	const [theme, setTheme] = useState("System");
	const text = useConceptText();

	return (
		<div className={cx("min-w-0 space-y-4", className)}>
			<div className="flex items-start justify-between gap-4">
				<div>
					<div className="flex items-center gap-2">
						<span className="flex h-8 w-8 items-center justify-center rounded-md border border-slate-200 bg-white text-slate-600">
							<Settings className="h-4 w-4" aria-hidden />
						</span>
						<p className="text-base font-semibold text-slate-900">{text("Operator preferences")}</p>
					</div>
					<p className="mt-2 max-w-2xl text-sm leading-6 text-slate-500">
						{text("Language and theme are app-level preferences because they change the Board experience itself.")}
					</p>
				</div>
				<span className="shrink-0 rounded-full bg-slate-100 px-2.5 py-1 text-xs font-medium text-slate-600">
					{text("Local setting")}
				</span>
			</div>
			<div className="grid gap-3 lg:grid-cols-2">
				<SettingChoiceGroup
					label="Language"
					options={["English", "中文", "日本語"]}
					value={language}
					onChange={setLanguage}
				/>
				<SettingChoiceGroup
					label="Theme"
					options={["System", "Light", "Dark"]}
					value={theme}
					onChange={setTheme}
				/>
			</div>
			<div className="rounded-lg border border-slate-200 bg-white p-4">
				<div className="grid gap-3 md:grid-cols-3">
					{[
						["Default client mode", "Hosted"],
						["Backup policy", "Before apply"],
						["Advanced nav", "Opt-in"],
					].map(([label, value]) => (
						<div key={label}>
							<p className="text-sm font-medium text-slate-500">{text(label)}</p>
							<p className="mt-1 text-lg font-semibold text-slate-950">{text(value)}</p>
						</div>
					))}
				</div>
			</div>
		</div>
	);
}

function AboutPreviewSurface({ className }: { className?: string }) {
	const text = useConceptText();

	return (
		<div className={cx("min-w-0 space-y-4", className)}>
			<div className="flex items-start justify-between gap-4">
				<div>
					<div className="flex items-center gap-2">
						<span className="flex h-8 w-8 items-center justify-center rounded-md border border-slate-200 bg-white text-slate-600">
							<BookText className="h-4 w-4" aria-hidden />
						</span>
						<p className="text-base font-semibold text-slate-900">{text("About MCPMate")}</p>
					</div>
					<p className="mt-2 max-w-2xl text-sm leading-6 text-slate-500">
						{text("The About surface carries release context, license posture, platform support, and the current operating boundary.")}
					</p>
				</div>
				<span className="shrink-0 rounded-full bg-sky-50 px-2.5 py-1 text-xs font-medium text-sky-700">
					{text("Skills channel")}
				</span>
			</div>
			<div className="grid gap-3 md:grid-cols-3">
				{[
					["Version", "Skills release"],
					["License", "AGPL-3.0"],
					["Platforms", "macOS, Windows, Linux"],
				].map(([label, value]) => (
					<div key={label} className="rounded-lg border border-slate-200 bg-white p-4">
						<p className="text-sm font-medium text-slate-500">{text(label)}</p>
						<p className="mt-2 text-xl font-semibold text-slate-950">{text(value)}</p>
					</div>
				))}
			</div>
			<div className="rounded-lg border border-slate-200 bg-white p-4">
				<p className="text-sm font-semibold text-slate-950">{text("Latest focus")}</p>
				<div className="mt-3 space-y-3">
					{[
						"Package capability as skill-shaped workflows.",
						"Preserve browser and Market import evidence.",
						"Keep enterprise gateway claims out of the current boundary.",
					].map((item) => (
						<div key={item} className="flex items-start gap-3">
							<CheckCircle2 className="mt-0.5 h-4 w-4 shrink-0 text-emerald-600" aria-hidden />
							<p className="text-sm leading-6 text-slate-500">{text(item)}</p>
						</div>
					))}
				</div>
			</div>
		</div>
	);
}

function DashboardPreviewCard({
	icon: Icon,
	title,
	rows,
}: {
	icon: LucideIcon;
	title: string;
	rows: Array<[string, string]>;
}) {
	const text = useConceptText();

	return (
		<div className="min-h-[9.75rem] rounded-lg border border-slate-200 bg-white p-4 transition hover:-translate-y-0.5 hover:border-sky-300">
			<div className="flex items-center justify-between">
				<p className="text-sm font-semibold text-slate-900">{text(title)}</p>
				<Icon className="h-4 w-4 text-slate-500" aria-hidden />
			</div>
			<div className="mt-6 space-y-2.5">
				{rows.map(([label, value]) => (
					<div key={label} className="flex items-center justify-between gap-3">
						<p className="text-sm text-slate-500">{text(label)}</p>
						{value === "Ready" ? (
							<span className="inline-flex items-center gap-1.5 rounded-full bg-emerald-500 px-3 py-1 text-xs font-semibold text-white">
								<span className="h-2 w-2 rounded-full bg-white/70" />
								{text(value)}
							</span>
						) : (
							<p className="text-sm font-medium text-slate-950">{text(value)}</p>
						)}
					</div>
				))}
			</div>
		</div>
	);
}

type PreviewChartVariant = "metrics" | "tokens";

type PreviewChartPoint = {
	time: string;
	cpu?: number;
	memory?: number;
	beforeFiltering?: number;
	afterFiltering?: number;
};

type PreviewChartSeries = {
	key: keyof Omit<PreviewChartPoint, "time">;
	name: string;
	color: string;
	dash?: string;
};

type PreviewChartConfig = {
	points: PreviewChartPoint[];
	series: PreviewChartSeries[];
	maxValue: number;
	tickValues: number[];
	ariaLabel: string;
};

const PREVIEW_CHART_VIEWBOX = {
	width: 520,
	height: 168,
	left: 46,
	right: 18,
	top: 6,
	bottom: 24,
} as const;

const previewMetricsData: PreviewChartPoint[] = [
	{ time: "11:22 AM", cpu: 0.03, memory: 0.28 },
	{ time: "02:55 PM", cpu: 0.03, memory: 0.26 },
	{ time: "01:17 AM", cpu: 0.03, memory: 0.33 },
	{ time: "05:04 PM", cpu: 0.03, memory: 0.3 },
	{ time: "09:12 PM", cpu: 0.03, memory: 0.29 },
	{ time: "01:15 PM", cpu: 0.03, memory: 0.31 },
];

const previewTokenData: PreviewChartPoint[] = [
	{ time: "11:22 AM", beforeFiltering: 0, afterFiltering: 0 },
	{ time: "07:58 AM", beforeFiltering: 0, afterFiltering: 4500 },
	{ time: "08:05 AM", beforeFiltering: 0, afterFiltering: 4500 },
	{ time: "02:23 PM", beforeFiltering: 0, afterFiltering: 0 },
	{ time: "05:01 PM", beforeFiltering: 0, afterFiltering: 6100 },
	{ time: "09:14 PM", beforeFiltering: 0, afterFiltering: 0 },
	{ time: "01:15 PM", beforeFiltering: 0, afterFiltering: 0 },
];

const previewMetricsSeries: PreviewChartSeries[] = [
	{ key: "cpu", name: "CPU (%)", color: "#3b82f6" },
	{ key: "memory", name: "Memory (%)", color: "#10b981", dash: "6 4" },
];

const previewTokenSeries: PreviewChartSeries[] = [
	{ key: "beforeFiltering", name: "Before Filtering", color: "#3b82f6", dash: "6 4" },
	{ key: "afterFiltering", name: "After Filtering", color: "#22c55e" },
];

const PREVIEW_CHART_PLOT_WIDTH = PREVIEW_CHART_VIEWBOX.width - PREVIEW_CHART_VIEWBOX.left - PREVIEW_CHART_VIEWBOX.right;
const PREVIEW_CHART_PLOT_HEIGHT = PREVIEW_CHART_VIEWBOX.height - PREVIEW_CHART_VIEWBOX.top - PREVIEW_CHART_VIEWBOX.bottom;

const previewChartConfigs: Record<PreviewChartVariant, PreviewChartConfig> = {
	metrics: {
		points: previewMetricsData,
		series: previewMetricsSeries,
		maxValue: 1.4,
		tickValues: [0, 0.35, 0.7, 1.36],
		ariaLabel: "Metrics trend chart preview",
	},
	tokens: {
		points: previewTokenData,
		series: previewTokenSeries,
		maxValue: 8000,
		tickValues: [0, 2000, 4000, 6000, 8000],
		ariaLabel: "Token savings trend chart preview",
	},
};

function chartX(index: number, points: PreviewChartPoint[]) {
	if (points.length <= 1) {
		return PREVIEW_CHART_VIEWBOX.left;
	}
	return PREVIEW_CHART_VIEWBOX.left + (PREVIEW_CHART_PLOT_WIDTH * index) / (points.length - 1);
}

function chartY(value: number, maxValue: number) {
	return PREVIEW_CHART_VIEWBOX.top + PREVIEW_CHART_PLOT_HEIGHT - (PREVIEW_CHART_PLOT_HEIGHT * value) / maxValue;
}

function formatPreviewChartValue(variant: PreviewChartVariant, value: number) {
	if (variant === "metrics") {
		return `${value.toFixed(2)}%`;
	}
	return value >= 1000 ? `${(value / 1000).toFixed(1)}K` : value.toString();
}

function buildPreviewChartPath(
	points: PreviewChartPoint[],
	seriesKey: PreviewChartSeries["key"],
	maxValue: number,
) {
	return points
		.map((point, index) => {
			const value = point[seriesKey] ?? 0;
			const command = index === 0 ? "M" : "L";
			return `${command}${chartX(index, points).toFixed(1)} ${chartY(value, maxValue).toFixed(1)}`;
		})
		.join(" ");
}

function PreviewChartLegend({ series }: { series: PreviewChartSeries[] }) {
	return (
		<div className="flex h-8 w-full shrink-0 flex-wrap items-center justify-center gap-x-4 gap-y-1 overflow-hidden px-2 text-xs font-medium leading-tight">
			{series.map((entry) => (
				<div key={entry.key} className="flex items-center gap-1.5" style={{ color: entry.color }}>
					<span className="inline-block h-2 w-2 rounded-full" style={{ backgroundColor: entry.color }} />
					<span>{entry.name}</span>
				</div>
			))}
		</div>
	);
}

function PreviewChartTooltip({
	activePoint,
	series,
	variant,
}: {
	activePoint: PreviewChartPoint;
	series: PreviewChartSeries[];
	variant: PreviewChartVariant;
}) {
	return (
		<div className="absolute right-3 top-3 z-10 rounded-md border border-slate-700 bg-slate-950 px-3 py-2 text-xs text-slate-100 shadow-lg">
			<div className="mb-1 text-[11px] text-slate-400">{activePoint.time}</div>
			<div className="space-y-1">
				{series.map((entry) => {
					const value = activePoint[entry.key] ?? 0;
					return (
						<div key={entry.key} className="flex items-center justify-between gap-4">
							<span className="flex items-center gap-2 text-[11px]" style={{ color: entry.color }}>
								<span className="inline-block h-2 w-2 rounded-full" style={{ backgroundColor: entry.color }} />
								{entry.name}
							</span>
							<span className="min-w-[3rem] text-right text-[11px] font-semibold text-slate-50">
								{formatPreviewChartValue(variant, value)}
							</span>
						</div>
					);
				})}
			</div>
		</div>
	);
}

function PreviewLineChart({ variant }: { variant: PreviewChartVariant }) {
	const [activeIndex, setActiveIndex] = useState<number | null>(null);
	const { ariaLabel, maxValue, points, series, tickValues } = previewChartConfigs[variant];
	const activePoint = activeIndex === null ? null : points[activeIndex];
	const visibleTimeLabels = points.filter((_, index) => index === 0 || index === points.length - 1 || index % 2 === 1);

	function updateActivePoint(event: PointerEvent<HTMLDivElement>) {
		const bounds = event.currentTarget.getBoundingClientRect();
		const plotLeftRatio = PREVIEW_CHART_VIEWBOX.left / PREVIEW_CHART_VIEWBOX.width;
		const plotWidthRatio = PREVIEW_CHART_PLOT_WIDTH / PREVIEW_CHART_VIEWBOX.width;
		const xRatio = Math.min(Math.max((event.clientX - bounds.left) / bounds.width, plotLeftRatio), plotLeftRatio + plotWidthRatio);
		const index = Math.round(((xRatio - plotLeftRatio) / plotWidthRatio) * (points.length - 1));
		setActiveIndex(Math.min(Math.max(index, 0), points.length - 1));
	}

	return (
		<div
			className="relative flex h-[190px] min-h-[190px] max-h-[190px] w-full shrink-0 flex-col overflow-hidden"
			onPointerMove={updateActivePoint}
			onPointerLeave={() => setActiveIndex(null)}
			onFocus={() => setActiveIndex(points.length - 1)}
			onBlur={() => setActiveIndex(null)}
			tabIndex={0}
			role="img"
			aria-label={ariaLabel}
		>
			{activePoint ? <PreviewChartTooltip activePoint={activePoint} series={series} variant={variant} /> : null}
			<svg className="min-h-0 w-full flex-1 overflow-visible" viewBox={`0 0 ${PREVIEW_CHART_VIEWBOX.width} ${PREVIEW_CHART_VIEWBOX.height}`} aria-hidden>
				{tickValues.map((tick) => {
					const y = chartY(tick, maxValue);
					return (
						<g key={tick}>
							<line
								x1={PREVIEW_CHART_VIEWBOX.left}
								x2={PREVIEW_CHART_VIEWBOX.width - PREVIEW_CHART_VIEWBOX.right}
								y1={y}
								y2={y}
								stroke="currentColor"
								strokeDasharray="3 3"
								className="text-slate-200"
							/>
							<text x="8" y={y + 4} className="fill-slate-500 text-[11px]">
								{formatPreviewChartValue(variant, tick)}
							</text>
						</g>
					);
				})}
				<line
					x1={PREVIEW_CHART_VIEWBOX.left}
					x2={PREVIEW_CHART_VIEWBOX.left}
					y1={PREVIEW_CHART_VIEWBOX.top}
					y2={PREVIEW_CHART_VIEWBOX.top + PREVIEW_CHART_PLOT_HEIGHT}
					stroke="currentColor"
					className="text-slate-300"
				/>
				{visibleTimeLabels.map((point) => {
					const index = points.indexOf(point);
					return (
						<text key={`${variant}-${point.time}-${index}`} x={chartX(index, points) - 18} y={PREVIEW_CHART_VIEWBOX.height - 6} className="fill-slate-500 text-[11px]">
							{point.time}
						</text>
					);
				})}
				{series.map((entry) => (
					<path
						key={entry.key}
						d={buildPreviewChartPath(points, entry.key, maxValue)}
						fill="none"
						stroke={entry.color}
						strokeWidth="2"
						strokeDasharray={entry.dash}
						strokeLinecap="round"
						strokeLinejoin="round"
					/>
				))}
				{activeIndex !== null ? (
					<>
						<line
							x1={chartX(activeIndex, points)}
							x2={chartX(activeIndex, points)}
							y1={PREVIEW_CHART_VIEWBOX.top}
							y2={PREVIEW_CHART_VIEWBOX.top + PREVIEW_CHART_PLOT_HEIGHT}
							stroke="currentColor"
							strokeDasharray="4 4"
							className="text-slate-400"
						/>
						{series.map((entry) => {
							const value = points[activeIndex][entry.key] ?? 0;
							return (
								<circle
									key={`${entry.key}-active`}
									cx={chartX(activeIndex, points)}
									cy={chartY(value, maxValue)}
									r="4"
									fill={entry.color}
									stroke="white"
									strokeWidth="2"
								/>
							);
						})}
					</>
				) : null}
			</svg>
			<PreviewChartLegend series={series} />
		</div>
	);
}

function DashboardPreviewSurface({ className }: { className?: string }) {
	const text = useConceptText();

	return (
		<div className={cx("min-w-0 space-y-5", className)}>
			<div className="flex w-full items-center justify-between gap-4 px-1 pb-1 pt-0">
				<div className="min-w-0 space-y-0.5">
					<div className="flex items-center gap-2">
						<span className="text-base font-semibold text-slate-900">{text("Local Core")}</span>
						<span className="text-sm text-slate-500">{text("Desktop")}</span>
					</div>
					<p className="flex flex-wrap items-baseline gap-x-2 gap-y-0 leading-5">
						<span className="text-base text-slate-700">{text("Running")}</span>
						<span className="text-sm text-slate-500">{text("MCPMate Desktop is managing the local core and will stop it when the app quits.")}</span>
					</p>
				</div>
				<div className="flex shrink-0 items-center gap-3">
					<button type="button" className="flex h-8 w-8 items-center justify-center rounded-full text-slate-500 hover:bg-slate-100" aria-label={text("Refresh core status")}>
						<RefreshCw className="h-4 w-4" aria-hidden />
					</button>
					<button type="button" className="flex h-11 w-11 items-center justify-center rounded-full bg-red-600 text-white" aria-label={text("Stop local core")}>
						<Square className="h-5 w-5" aria-hidden />
					</button>
				</div>
			</div>

			<div className="grid gap-4 md:grid-cols-2 lg:grid-cols-4">
				<DashboardPreviewCard
					icon={Activity}
					title="System Status"
					rows={[
						["Status", "Ready"],
						["Uptime", "8s"],
						["Version", "v0.1.0"],
					]}
				/>
				<DashboardPreviewCard
					icon={SlidersHorizontal}
					title="Profiles"
					rows={[
						["Total Profiles", "1"],
						["Active Profiles", "1"],
					]}
				/>
				<DashboardPreviewCard
					icon={AppWindow}
					title="Clients"
					rows={[
						["Total Clients", "0"],
						["Approved", "0"],
					]}
				/>
				<DashboardPreviewCard
					icon={Server}
					title="Servers"
					rows={[
						["Total Servers", "4"],
						["Connected", "0"],
					]}
				/>
			</div>

			<div className="grid items-stretch gap-4 lg:grid-cols-2">
				<div className="flex h-full min-h-[19rem] flex-col rounded-lg border border-slate-200 bg-white">
					<div className="flex flex-col gap-1.5 p-4">
						<div className="flex items-center gap-2">
							<Activity className="h-5 w-5 text-sky-500" aria-hidden />
							<p className="text-base font-semibold text-slate-950">{text("Metrics")}</p>
						</div>
						<p className="text-xs leading-5 text-slate-500">{text("MCPMate process CPU and memory utilization sampled every 30 seconds")}</p>
					</div>
					<div className="min-h-0 flex-1 px-4 pb-3 pt-0">
						<PreviewLineChart variant="metrics" />
					</div>
				</div>
				<div className="flex h-full min-h-[19rem] flex-col rounded-lg border border-slate-200 bg-white">
					<div className="flex flex-col gap-1.5 p-4">
						<div className="flex items-center justify-between gap-3">
							<div className="flex min-w-0 items-center gap-2">
								<Sparkles className="h-5 w-5 shrink-0 text-amber-500" aria-hidden />
								<p className="truncate text-base font-semibold text-slate-950">{text("Token Savings")}</p>
							</div>
							<p className="shrink-0 text-xs font-semibold text-emerald-600">↗ 0 saved</p>
						</div>
						<p className="text-xs leading-5 text-slate-500">{text("Estimated context savings from profile filtering")}</p>
					</div>
					<div className="min-h-0 flex-1 px-4 pb-3 pt-0">
						<PreviewLineChart variant="tokens" />
					</div>
				</div>
			</div>
		</div>
	);
}

function ModulePreviewSurface({
	moduleKey,
	className,
}: {
	moduleKey: DesktopModuleKey;
	className?: string;
}) {
	const text = useConceptText();
	const module = desktopModules[moduleKey];
	const ModuleIcon = module.icon;

	if (moduleKey === "dashboard") {
		return <DashboardPreviewSurface className={className} />;
	}

	if (moduleKey === "settings") {
		return <SettingsPreviewSurface className={className} />;
	}

	if (moduleKey === "about") {
		return <AboutPreviewSurface className={className} />;
	}

	return (
		<div className={cx("min-w-0 space-y-4", className)}>
			<div className="flex items-start justify-between gap-4">
				<div className="min-w-0">
					<div className="flex items-center gap-2">
						<span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-md border border-slate-200 bg-white text-slate-600">
							<ModuleIcon className="h-4 w-4" aria-hidden />
						</span>
						<p className="truncate text-base font-semibold text-slate-900">{text(module.title)}</p>
					</div>
					<p className="mt-2 max-w-2xl text-sm leading-6 text-slate-500">{text(module.body)}</p>
				</div>
				<span className="shrink-0 rounded-full bg-emerald-50 px-2.5 py-1 text-xs font-medium text-emerald-700">
					{text(module.status)}
				</span>
			</div>

			<div className="grid gap-3 md:grid-cols-3">
				{module.stats.map(([label, value]) => (
					<div key={label} className="rounded-lg border border-slate-200 bg-white p-4">
						<p className="text-sm font-medium text-slate-500">{text(label)}</p>
						<p className="mt-2 text-2xl font-bold text-slate-950">{text(value)}</p>
					</div>
				))}
			</div>

			<div className="overflow-hidden rounded-lg border border-slate-200 bg-white">
				{module.rows.map(([label, value]) => (
					<div key={label} className="grid gap-2 border-b border-slate-200 px-4 py-3 last:border-b-0 md:grid-cols-[10rem_1fr]">
						<p className="text-sm font-medium text-slate-900">{text(label)}</p>
						<p className="text-sm leading-6 text-slate-500">{text(value)}</p>
					</div>
				))}
			</div>
		</div>
	);
}

function DesktopSidebarModuleButton({
	moduleKey,
	activeModule,
	onModuleChange,
}: DesktopSidebarModuleButtonProps) {
	const text = useConceptText();
	const item = desktopModules[moduleKey];
	const ItemIcon = item.icon;
	const isActive = moduleKey === activeModule;
	const label = text(item.label);

	return (
		<button
			type="button"
			onClick={() => onModuleChange(moduleKey)}
			className={cx(
				"flex w-full items-center justify-center gap-3 rounded-md px-0 py-2 text-left text-sm font-medium transition xl:justify-start xl:px-3",
				isActive ? "bg-slate-100 text-slate-950" : "text-slate-500 hover:bg-slate-100 hover:text-slate-950",
			)}
			aria-pressed={isActive}
			aria-label={label}
			title={label}
		>
			<ItemIcon className="h-5 w-5 shrink-0" aria-hidden />
			<span className="hidden xl:inline">{label}</span>
		</button>
	);
}

function DesktopSidebarModuleGroup({
	label,
	moduleKeys,
	activeModule,
	onModuleChange,
}: DesktopSidebarModuleGroupProps) {
	const text = useConceptText();

	return (
		<div>
			{label ? <p className="mb-1 hidden px-3 text-xs font-semibold text-slate-400 xl:block">{text(label)}</p> : null}
			{moduleKeys.map((moduleKey) => (
				<DesktopSidebarModuleButton
					key={moduleKey}
					moduleKey={moduleKey}
					activeModule={activeModule}
					onModuleChange={onModuleChange}
				/>
			))}
		</div>
	);
}

function DesktopSimulation({
	className,
	initialModule = "dashboard",
}: {
	className?: string;
	initialModule?: DesktopModuleKey;
}) {
	const [activeModule, setActiveModule] = useState<DesktopModuleKey>(initialModule);
	const text = useConceptText();
	const module = desktopModules[activeModule];

	return (
		<div
			className={cx(
				"flex h-[42.5rem] flex-col overflow-hidden rounded-xl border border-slate-200 bg-white shadow-[0_34px_110px_rgba(15,23,42,0.16)]",
				className,
			)}
		>
			<div className="flex h-7 shrink-0 items-center gap-2 bg-slate-100 px-3">
				<span className="h-3 w-3 rounded-full bg-red-500" />
				<span className="h-3 w-3 rounded-full bg-amber-400" />
				<span className="h-3 w-3 rounded-full bg-emerald-500" />
			</div>
			<div className="grid min-h-0 flex-1 bg-slate-50 grid-cols-[4rem_1fr] xl:grid-cols-[16rem_1fr]">
				<aside className="flex min-h-0 flex-col border-r border-slate-200 bg-white">
					<div className="flex h-16 shrink-0 items-center justify-center gap-2 px-2 xl:justify-start xl:px-4">
						<img src={logoImage} alt="MCPMate" className="h-7 w-7 object-contain dark:brightness-0 dark:invert" />
						<div className="hidden min-w-0 flex-1 xl:block">
							<p className="truncate text-xl font-bold text-slate-950">
								MCPMate <sup className="text-[9px] font-medium text-slate-400">Beta</sup>
							</p>
						</div>
						<button type="button" className="hidden h-8 w-8 items-center justify-center rounded-md text-slate-500 hover:bg-slate-100 xl:flex" aria-label={text("Collapse sidebar")}>
							<Menu className="h-5 w-5" aria-hidden />
						</button>
					</div>
					<div className="flex-1 space-y-5 px-2 py-4">
						<DesktopSidebarModuleGroup
							label="MAIN"
							moduleKeys={desktopMainModuleOrder}
							activeModule={activeModule}
							onModuleChange={setActiveModule}
						/>
						<DesktopSidebarModuleGroup
							label="Advanced"
							moduleKeys={desktopAdvancedModuleOrder}
							activeModule={activeModule}
							onModuleChange={setActiveModule}
						/>
					</div>
					<div className="space-y-1 border-t border-slate-200 p-2">
						<DesktopSidebarModuleGroup
							moduleKeys={desktopFooterModuleOrder}
							activeModule={activeModule}
							onModuleChange={setActiveModule}
						/>
					</div>
				</aside>

				<div className="flex min-h-0 min-w-0 flex-col">
					<header className="flex h-16 items-center justify-between border-b border-slate-200 bg-white px-4">
						<div className="min-w-0">
							<p className="truncate text-xl font-semibold text-slate-950">{text(module.label)}</p>
						</div>
						<div className="flex items-center gap-2">
							<BoardHeaderIconButton icon={MessageSquare} label="Open GitHub Discussions" />
							<BoardHeaderIconButton icon={BookOpen} label="Open documentation" />
							<BoardHeaderIconButton icon={Moon} label="Toggle theme" />
							<BoardHeaderIconButton icon={Bell} label="Notifications" />
						</div>
					</header>

					<div className="min-h-0 flex-1 space-y-4 overflow-y-auto p-4">
						<ModulePreviewSurface moduleKey={activeModule} />
					</div>
				</div>
			</div>
		</div>
	);
}

function HeroSection() {
	const text = useConceptText();

	return (
		<section className="overflow-hidden bg-white pt-24 text-zinc-950">
			<div className="relative mx-auto max-w-7xl px-5 pb-20 pt-8 md:px-8">
				<DesktopSimulation />
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
