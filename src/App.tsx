import { type ReactNode, useEffect, useMemo, useState, useSyncExternalStore } from "react";
import {
  AlertTriangle,
  BellRing,
  ChevronDown,
  ChevronUp,
  Copy,
  CreditCard,
  Download,
  ExternalLink,
  KeyRound,
  LogIn,
  Pause,
  Play,
  Plus,
  RefreshCw,
  Send,
  Trash2,
  UserPlus,
  X,
} from "lucide-react";

import { Combobox } from "@/components/Combobox";
import { MultiCombobox } from "@/components/MultiCombobox";
import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { ScrollArea } from "@/components/ui/scroll-area";
import { Switch } from "@/components/ui/switch";
import {
  capacityOptions,
  colorOptions,
  familyOptions,
  keepAvailable,
  productsForSelection,
} from "@/lib/product-selection";
import {
  cleanPushRows,
  PUSH_KIND_LABEL,
  type PushKind,
  pushSummary,
  samePushUrls,
  savedPushAddresses,
} from "@/lib/push";
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from "@/components/ui/table";
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from "@/components/ui/tooltip";

import {
  addAccount,
  changeLocale,
  checkAccountSession,
  clearAccountSession,
  connect,
  copyText,
  copyPaymentLink,
  deleteAccount,
  dismissUpdate,
  installUpdate,
  openAccountLogin,
  openPaymentLink,
  placeOrder,
  refreshProducts,
  saveAccountSession,
  saveAccountLoginCookies,
  saveSettings,
  selectAccount,
  setCategory,
  setIntervalSeconds,
  setPushUrls,
  setTargets,
  startAllAccounts,
  startWatching,
  stopAllAccounts,
  stopWatching,
  testNotify,
  updateAccount,
  watcherStore,
} from "@/lib/store";
import {
  type Account,
  type AccountPatch,
  type Availability,
  type Category,
  describeAdvice,
  describeAvailability,
  formatTime,
  isUntrusted,
  type OrderRecord,
  type Product,
  type SessionStatus,
  type Settings,
  type StatusTone,
  type Target,
  targetKey,
} from "@/lib/types";
import { cn } from "@/lib/utils";

/** 四种展示状态各自的样式。 */
const TONE_CLASS: Record<StatusTone, string> = {
  inStock: "bg-in-stock/15 text-in-stock border-in-stock/30 font-medium",
  outOfStock: "bg-muted text-muted-foreground border-transparent",
  // 「未知」必须和「无货」长得完全不一样。这是整个项目的意义所在：上游把查询
  // 失败显示成「无货」，用户对着一个早已失效的程序空等了大半年。
  unknown: "bg-unknown/15 text-unknown border-unknown/40 font-medium",
  pending: "bg-transparent text-muted-foreground/60 border-dashed",
};

function StatusBadge({ availability }: { availability: Availability }) {
  const { label, tone, detail } = describeAvailability(availability);
  const badge = (
    <Badge variant="outline" className={`min-w-18 justify-center ${TONE_CLASS[tone]}`}>
      {label}
    </Badge>
  );
  if (!detail) return badge;
  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <span className="cursor-help">{badge}</span>
      </TooltipTrigger>
      <TooltipContent className="max-w-90">{detail}</TooltipContent>
    </Tooltip>
  );
}

/**
 * 一块带标题的分区。「在哪儿加什么」和「怎么跑」分开放，控件不再挤成一排。
 *
 * 传 `onToggle` 就可以折叠：标题行整行可点，折叠后只剩标题和一行摘要。
 */
function Panel({
  title,
  hint,
  open = true,
  onToggle,
  children,
}: {
  title: string;
  hint?: string;
  open?: boolean;
  onToggle?: () => void;
  children: ReactNode;
}) {
  const heading = (
    <>
      <h2 className="shrink-0 text-sm font-medium">{title}</h2>
      {hint !== undefined && (
        <p className="text-muted-foreground min-w-0 flex-1 truncate text-xs" title={hint}>
          {hint}
        </p>
      )}
      {onToggle !== undefined &&
        (open ? (
          <ChevronUp className="text-muted-foreground size-4 shrink-0" />
        ) : (
          <ChevronDown className="text-muted-foreground size-4 shrink-0" />
        ))}
    </>
  );
  return (
    <section className="bg-card text-card-foreground rounded-xl border shadow-xs">
      {onToggle !== undefined ? (
        <button
          type="button"
          aria-expanded={open}
          onClick={onToggle}
          className={cn(
            "hover:bg-muted/40 flex w-full items-center gap-3 px-4 py-2 text-left",
            open ? "rounded-t-xl" : "rounded-xl",
          )}
        >
          {heading}
        </button>
      ) : (
        <div className="flex items-center gap-3 px-4 py-2">{heading}</div>
      )}
      {open && <div className="grid gap-2.5 border-t px-4 pt-3 pb-4">{children}</div>}
    </section>
  );
}

const SETTINGS_OPEN_KEY = "apw.settingsOpen";

/** 上次是展开还是收起。读不到（首次启动、无 localStorage）按展开。 */
function readSettingsOpen(): boolean {
  try {
    return localStorage.getItem(SETTINGS_OPEN_KEY) !== "0";
  } catch {
    return true;
  }
}

/** 收起时的一行摘要：设置折起来了，但它们是什么一眼还在。 */
function settingsSummary(s: Settings): string {
  return [
    `每 ${s.intervalSeconds} 秒查一轮`,
    pushSummary(savedPushAddresses(s)),
    (s.proxies ?? []).length > 0 ? `代理 ${s.proxies.length} 条` : "无代理",
    s.soundEnabled ? "提示音开" : "提示音关",
    s.openBagOnHit ? "有货时开购物袋" : "有货时不开购物袋",
  ].join(" · ");
}

/** 运行状态。一个会呼吸的点比一行灰字更像「它还活着」。 */
function RunState({ running }: { running: boolean }) {
  return (
    <span className="text-muted-foreground flex items-center gap-2 text-sm">
      <span className="relative flex size-2">
        {running && (
          <span className="bg-in-stock/60 absolute inline-flex size-full animate-ping rounded-full [animation-duration:2.4s]" />
        )}
        <span
          className={cn(
            "relative inline-flex size-2 rounded-full",
            running ? "bg-in-stock" : "bg-muted-foreground/40",
          )}
        />
      </span>
      {running ? "监听中" : "已暂停"}
    </span>
  );
}

/** 监控列表的筛选按钮。数字等宽：每轮查询都会变，几个并排时不该来回抖。 */
function FilterChip({
  label,
  count,
  active,
  tone,
  onClick,
}: {
  label: string;
  count: number;
  active: boolean;
  tone?: "inStock" | "unknown";
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      aria-label={`筛选：${label}`}
      aria-pressed={active}
      onClick={onClick}
      className={cn(
        "inline-flex items-baseline gap-1.5 rounded-md border px-2.5 py-1 text-xs transition-colors",
        active
          ? "border-primary bg-accent text-foreground"
          : "bg-card text-muted-foreground hover:bg-accent/60",
      )}
    >
      <span>{label}</span>
      <span
        className={cn(
          "font-semibold tabular-nums",
          count > 0 && tone === "inStock" && "text-in-stock",
          count > 0 && tone === "unknown" && "text-unknown",
        )}
      >
        {count}
      </span>
    </button>
  );
}

/** 发给客户的订单文案：商品、门店和付款链接一眼能看全。 */
function describeOrderMessage(order: OrderRecord): string {
  return [
    "【Apple 到店取货】",
    order.productName,
    `门店：${order.storeTitle}`,
    `付款链接：${order.paymentUrl}`,
    "请在 10 分钟内用同一浏览器会话完成付款。",
  ].join("\n");
}

/** 日志行。时间戳弱化，到货那几行用有货色标出来，一屏灰字里一眼能找到。 */
function LogLine({ line }: { line: string }) {
  const match = /^\[(\d{2}:\d{2}:\d{2})\] ?(.*)$/s.exec(line);
  if (!match) return <div className="whitespace-pre-wrap">{line}</div>;
  const [, stamp, text = ""] = match;
  return (
    <div className="whitespace-pre-wrap">
      <span className="text-muted-foreground/60">{stamp}</span>{" "}
      <span className={text.startsWith("有货") ? "text-in-stock font-medium" : "text-muted-foreground"}>
        {text}
      </span>
    </div>
  );
}

/** 会话状态的展示色。已登录是绿，已失效是红，其余是中性的「不知道」。 */
const SESSION_CLASS: Record<SessionStatus, string> = {
  signed_in: "bg-in-stock/15 text-in-stock border-in-stock/30",
  guest: "bg-unknown/10 text-unknown border-unknown/30",
  expired: "bg-destructive/10 text-destructive border-destructive/30",
  unknown: "bg-muted text-muted-foreground border-transparent",
  not_configured: "bg-transparent text-muted-foreground border-dashed",
};

function SessionBadge({ status, label }: { status: SessionStatus; label: string }) {
  return (
    <Badge variant="outline" className={SESSION_CLASS[status]}>
      {label}
    </Badge>
  );
}

export default function App() {
  const ui = useSyncExternalStore(watcherStore.subscribe, watcherStore.getSnapshot);

  useEffect(() => {
    void connect();
    // 刻意不在清理函数里断开：这是应用级的单一连接，窗口活着它就该活着。
    // StrictMode 的重复调用由 connect 内部去重。
  }, []);

  // 门店可以一次选好几家：同城门店合并成一次请求之后，多选不增加请求量。
  const [storeNumbers, setStoreNumbers] = useState<string[]>([]);
  // 非 iPhone 品类直接选一个完整型号。
  const [partNumber, setPartNumber] = useState("");
  // iPhone 按机型 → 容量 → 颜色三级选择，三级都可多选：发售当晚常见的是
  // 「这两款、这三个颜色，哪个有货要哪个」，一台一台加太慢。三级组合里目录中
  // 存在的型号全部加入。
  const [families, setFamilies] = useState<string[]>([]);
  const [capacities, setCapacities] = useState<string[]>([]);
  const [colors, setColors] = useState<string[]>([]);
  // 推送地址：Bark 和飞书是同一个列表，一行一个。null 表示还没动过，跟着后端走；
  // 动过之后以这份为准，保持用户填写的顺序，渠道标签按后端的归类显示。
  const [pushDraft, setPushDraft] = useState<string[] | null>(null);
  // 代理地址和推送地址一样一行一个，草稿的规矩也相同，见 pushDraft。
  const [proxyDraft, setProxyDraft] = useState<string[] | null>(null);
  const [intervalDraft, setIntervalDraft] = useState<number | null>(null);
  // 设置改一次就放着不动，折起来把纵向空间还给表格；记住上次的选择。
  const [settingsOpen, setSettingsOpen] = useState(readSettingsOpen());
  // 账号面板默认展开：新用户第一件事就是选账号、贴 Cookie。
  const [accountsOpen, setAccountsOpen] = useState(true);
  const [newAccountName, setNewAccountName] = useState("");
  const [newAccountAppleId, setNewAccountAppleId] = useState("");
  // Cookie 草稿只存在内存里。已保存的原值永远不回显，避免截图和肩窥。
  const [sessionDraft, setSessionDraft] = useState("");
  // 监控列表按状态筛选：统计数字和列表是同一份数据，不另开一套视图。
  const [statusFilter, setStatusFilter] = useState<
    "all" | "in_stock" | "out_of_stock" | "unknown"
  >("all");
  function toggleSettings() {
    const next = !settingsOpen;
    setSettingsOpen(next);
    try {
      localStorage.setItem(SETTINGS_OPEN_KEY, next ? "1" : "0");
    } catch {
      // 存不下就每次都展开，不值得报错。
    }
  }

  // 兼容旧状态形状：没有账号字段时按空列表处理，界面仍能走旧版流程。
  const accounts = ui.accounts ?? [];
  const selectedAccount =
    accounts.find((account) => account.id === ui.selectedAccountId) ?? null;

  // 每个账号的最近一次加购结果汇总成一份订单列表，按时间倒序。
  const orderList = useMemo(() => {
    const known = ui.orders ?? {};
    return accounts
      .map((account) => ({
        accountName: account.name,
        order: known[account.id] ?? account.lastOrder,
      }))
      .filter(
        (item): item is { accountName: string; order: OrderRecord } => item.order !== null,
      )
      .sort((a, b) => b.order.attemptedAtMs - a.order.attemptedAtMs);
  }, [accounts, ui.orders]);

  const visibleRows = useMemo(
    () => {
      if (statusFilter === "all") return ui.rows;
      // 「查不到」只算真正查失败的项目，和统计数字保持同一口径；
      // 还没轮到查询的「待查询」不算问题。
      if (statusFilter === "unknown") {
        return ui.rows.filter((row) => isUntrusted(row.availability));
      }
      return ui.rows.filter((row) => row.availability.kind === statusFilter);
    },
    [ui.rows, statusFilter],
  );

  // 切换账号后清掉上一个账号的 Cookie 草稿，免得把 A 的会话存进 B。
  useEffect(() => {
    setSessionDraft("");
  }, [ui.selectedAccountId]);

  const savedPush = useMemo(() => savedPushAddresses(ui.settings), [ui.settings]);
  const savedPushKind = new Map<string, PushKind>(savedPush.map((p) => [p.url, p.kind]));
  const pushRows = pushDraft ?? savedPush.map((p) => p.url);
  // 一个地址都没有时留一个空行，直接粘贴就行，不用先点「添加」。
  const visiblePushRows = pushRows.length > 0 ? pushRows : [""];
  const lastPushRow = visiblePushRows[visiblePushRows.length - 1] ?? "";

  function editPushRow(index: number, value: string) {
    setPushDraft(visiblePushRows.map((row, i) => (i === index ? value : row)));
  }

  // 失焦和删除都走这里：整理后整组交给后端归类保存；没有实际改动就不写盘。
  // 整理后的列表留作草稿：保存是异步的，这时退回「跟着后端走」，界面会先闪回
  // 旧地址，紧接着点「添加」还会以旧列表为底，把刚填的地址弄丢。
  function commitPushRows(rows: string[]) {
    const urls = cleanPushRows(rows);
    setPushDraft(urls);
    if (!samePushUrls(urls, savedPush.map((p) => p.url))) {
      void setPushUrls(urls);
    }
  }
  // 旧设置文件没有 proxies 字段，读上来是 undefined，当作空列表。
  const savedProxies = ui.settings.proxies ?? [];
  const proxyRows = proxyDraft ?? savedProxies;
  const visibleProxyRows = proxyRows.length > 0 ? proxyRows : [""];
  const lastProxyRow = visibleProxyRows[visibleProxyRows.length - 1] ?? "";

  function editProxyRow(index: number, value: string) {
    setProxyDraft(visibleProxyRows.map((row, i) => (i === index ? value : row)));
  }

  // 每个代理是一条额外的出口线路：Apple 的配额按出口 IP 计，多一条线路多一份配额，
  // 被拦时自动换下一条。后端只留能解析的地址，留不下的那行会标「未生效」。
  function commitProxyRows(rows: string[]) {
    const proxies = cleanPushRows(rows);
    setProxyDraft(proxies);
    if (!samePushUrls(proxies, savedProxies)) {
      void saveSettings({ ...ui.settings, proxies });
    }
  }
  const intervalValue = intervalDraft ?? ui.settings.intervalSeconds;

  const storeOptions = useMemo(
    () => ui.stores.map((s) => ({ value: s.number, label: s.title })),
    [ui.stores],
  );
  // 只列当前品类。四个品类的型号加起来好几百条，全堆进一个下拉框，
  // 想找一台 Mac 得先划过所有 iPhone。
  const productOptions = useMemo(
    () =>
      ui.products
        .filter((p) => p.category === ui.category)
        .map((p) => ({ value: p.partNumber, label: p.title })),
    [ui.products, ui.category],
  );
  const iphoneFamilyOptions = useMemo(
    () => familyOptions(ui.products, ui.category),
    [ui.products, ui.category],
  );
  const iphoneCapacityOptions = useMemo(
    () => capacityOptions(ui.products, ui.category, families),
    [ui.products, ui.category, families],
  );
  const iphoneColorOptions = useMemo(
    () => colorOptions(ui.products, ui.category, families, capacities),
    [ui.products, ui.category, families, capacities],
  );
  // 点「添加」时会加入的型号。iPhone 是三级选择的组合，其他品类就是选中的那一个。
  const selectedProducts = useMemo<Product[]>(() => {
    if (ui.category === "iphone") {
      return productsForSelection(ui.products, ui.category, families, capacities, colors);
    }
    const product = ui.products.find((p) => p.partNumber === partNumber);
    return product ? [product] : [];
  }, [ui.products, ui.category, families, capacities, colors, partNumber]);

  function resetProductSelection() {
    setPartNumber("");
    setFamilies([]);
    setCapacities([]);
    setColors([]);
  }

  // 上级变了，下级只去掉不再可选的项，仍然成立的留着（理由见 keepAvailable）。
  function onFamiliesChange(next: string[]) {
    setFamilies(next);
    const nextCapacities = keepAvailable(
      capacities,
      capacityOptions(ui.products, ui.category, next),
    );
    setCapacities(nextCapacities);
    setColors(
      keepAvailable(colors, colorOptions(ui.products, ui.category, next, nextCapacities)),
    );
  }

  function onCapacitiesChange(next: string[]) {
    setCapacities(next);
    setColors(keepAvailable(colors, colorOptions(ui.products, ui.category, families, next)));
  }

  const targets = useMemo(() => ui.rows.map((r) => r.target), [ui.rows]);

  const summary = useMemo(() => {
    let inStock = 0;
    let outOfStock = 0;
    let untrusted = 0;
    for (const r of ui.rows) {
      if (r.availability.kind === "in_stock") inStock += 1;
      else if (r.availability.kind === "out_of_stock") outOfStock += 1;
      if (isUntrusted(r.availability)) untrusted += 1;
    }
    return { inStock, outOfStock, untrusted };
  }, [ui.rows]);

  const canAdd = storeNumbers.length > 0 && selectedProducts.length > 0;

  async function onAdd() {
    if (!canAdd) return;

    // 门店 × 型号，每个组合一条目标；已经在列表里的跳过，不重复。
    // 按门店分组排，表格里同一家店的几台机器挨在一起。
    const existing = new Set(targets.map(targetKey));
    const added: Target[] = [];
    for (const number of storeNumbers) {
      const store = ui.stores.find((s) => s.number === number);
      if (!store) continue;
      for (const product of selectedProducts) {
        const next: Target = {
          locale: ui.settings.locale,
          storeNumber: store.number,
          storeTitle: store.title,
          partNumber: product.partNumber,
          productName: product.title,
          ...(product.companionPart ? { companionPart: product.companionPart } : {}),
        };
        const key = targetKey(next);
        if (existing.has(key)) continue;
        existing.add(key);
        added.push(next);
      }
    }
    if (added.length === 0) return;
    await setTargets([...targets, ...added]);
    // 只清型号，门店留着：接着给同一批门店加下一个型号是最常见的操作。
    resetProductSelection();
  }

  async function onRemove(t: Target) {
    await setTargets(targets.filter((x) => targetKey(x) !== targetKey(t)));
  }

  function patchAccount(account: Account, patch: AccountPatch) {
    void updateAccount(account.id, patch);
  }

  async function onCreateAccount() {
    const name = newAccountName.trim() || `账号 ${accounts.length + 1}`;
    const created = await addAccount(name, newAccountAppleId.trim(), ui.settings.locale);
    if (created) {
      setNewAccountName("");
      setNewAccountAppleId("");
      // 新账号创建后直接打开该账号专属的 Apple 登录窗口，用户登录完成再回来保存 Cookie。
      void openAccountLogin(created.id);
    }
  }

  async function onDeleteAccount(account: Account) {
    // 至少要留一个账号，否则界面没有一个可以承载监控目标的地方。
    if (accounts.length <= 1) return;
    const message = `删除账号「${account.name}」？本地保存的会话和监控目标会一并删除。`;
    const confirmed =
      typeof window === "undefined" || typeof window.confirm !== "function"
        ? true
        : window.confirm(message);
    if (confirmed) await deleteAccount(account.id);
  }

  async function onSaveSession() {
    if (!selectedAccount || sessionDraft.trim() === "") return;
    const saved = await saveAccountSession(selectedAccount.id, sessionDraft);
    // 保存成功后立刻丢掉草稿：原值不在界面上多停留一秒。
    if (saved) setSessionDraft("");
  }

  async function onSaveLoginCookies() {
    if (!selectedAccount) return;
    const saved = await saveAccountLoginCookies(selectedAccount.id);
    if (saved) {
      setSessionDraft("");
      // 保存后立刻校验一次，界面能直接显示「已登录 / 访客 / 已失效」。
      await checkAccountSession(saved.id);
    }
  }

  return (
    <TooltipProvider delayDuration={200}>
      <div className="mx-auto flex h-screen max-w-6xl flex-col gap-3 overflow-y-auto p-5">
        <header className="flex items-start justify-between gap-4">
          <div>
            <h1 className="text-xl font-semibold tracking-tight">Apple Pickup Watcher</h1>
            <p className="text-muted-foreground text-sm">
              多账号盯 Apple 直营店的到店取货库存，有货自动加购并给出付款链接
            </p>
          </div>
          <div className="flex flex-wrap items-center justify-end gap-2">
            {selectedAccount !== null && (
              <span className="text-muted-foreground text-sm">
                当前账号：{selectedAccount.name}
              </span>
            )}
            <RunState running={ui.running} />
            {ui.running ? (
              <Button variant="secondary" onClick={() => void stopWatching()}>
                <Pause /> 暂停
              </Button>
            ) : (
              <Button
                onClick={() => void startWatching()}
                disabled={ui.rows.length === 0 || selectedAccount?.enabled === false}
                title={
                  selectedAccount?.enabled === false
                    ? "账号已停用，先在账号面板里启用"
                    : undefined
                }
              >
                <Play /> 开始
              </Button>
            )}
            <Button
              variant="outline"
              size="sm"
              disabled={accounts.length === 0}
              onClick={() => void startAllAccounts()}
            >
              全部开始
            </Button>
            <Button
              variant="ghost"
              size="sm"
              disabled={accounts.length === 0}
              onClick={() => void stopAllAccounts()}
            >
              全部暂停
            </Button>
          </div>
        </header>

        <div className="grid shrink-0 gap-3 lg:min-h-0 lg:shrink lg:flex-1 lg:grid-cols-[minmax(0,1fr)_minmax(0,1.1fr)]">
          <div className="flex min-h-0 flex-col gap-3 lg:overflow-y-auto lg:pr-1">
          {orderList.length > 0 && (
          // 付款这一步刻意留给用户：链接只把用户带到自己的购物袋，程序不接触
          // 支付方式，也不替用户确认账单信息。多账号时按时间列出每一单。
          <section className="bg-card grid gap-2 rounded-xl border px-4 py-3 shadow-xs">
            <div className="flex flex-wrap items-baseline gap-x-2 gap-y-1">
              <h2 className="text-sm font-medium">订单与付款链接</h2>
              <p className="text-muted-foreground text-xs">
                到货后自动加入对应账号的购物袋；打开或复制链接交给客户付款。
              </p>
            </div>
            <div className="grid gap-2">
              {orderList.map(({ accountName, order }) => (
                <div
                  key={order.id}
                  className={cn(
                    "grid gap-1.5 rounded-lg border px-3 py-2.5 text-sm",
                    order.status === "added"
                      ? "border-in-stock/40 bg-in-stock/10"
                      : "border-destructive/30 bg-destructive/5",
                  )}
                >
                  <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
                    {order.status === "added" ? (
                      <CreditCard className="text-in-stock size-4" />
                    ) : (
                      <AlertTriangle className="text-destructive size-4" />
                    )}
                    <p
                      className={cn(
                        "font-medium",
                        order.status === "added" ? "text-in-stock" : "text-destructive",
                      )}
                    >
                      {order.status === "added" ? "已加入购物袋，等待付款" : "自动加购失败"}
                    </p>
                    <Badge variant="outline" className="text-muted-foreground">
                      {accountName}
                    </Badge>
                    <span className="text-muted-foreground text-xs">
                      {formatTime(order.attemptedAtMs)}
                    </span>
                    {order.status === "added" && order.bagQuantity !== null && (
                      <Badge variant="outline" className="text-muted-foreground">
                        购物袋 {order.bagQuantity} 件
                      </Badge>
                    )}
                    {order.paymentUrl !== "" && (
                      <div className="ml-auto flex flex-wrap items-center gap-1.5">
                        <Button size="sm" onClick={() => void openPaymentLink(order.paymentUrl)}>
                          <ExternalLink /> 打开付款
                        </Button>
                        <Button
                          size="sm"
                          variant="secondary"
                          onClick={() => void copyPaymentLink(order.paymentUrl)}
                        >
                          <Copy /> 复制链接
                        </Button>
                        <Button
                          size="sm"
                          variant="outline"
                          onClick={() =>
                            void copyText(
                              describeOrderMessage(order),
                              "订单与付款链接已复制，直接粘贴发给客户即可。",
                            )
                          }
                        >
                          <Send /> 发给用户
                        </Button>
                      </div>
                    )}
                  </div>
                  <p>
                    {order.productName}
                    <span className="text-muted-foreground"> · {order.storeTitle}</span>
                  </p>
                  {order.paymentUrl !== "" && (
                    <p
                      aria-label="付款链接"
                      className="text-muted-foreground font-mono text-xs break-all select-text"
                    >
                      {order.paymentUrl}
                    </p>
                  )}
                  <p className="text-muted-foreground text-xs">{order.message}</p>
                </div>
              ))}
            </div>
            <p className="text-muted-foreground text-xs">
              付款链接绑定导入 Cookie 的同一浏览器会话。换浏览器、换设备或清掉 Cookie
              后打开，可能只看到一个空购物袋。
            </p>
          </section>
        )}

        {ui.trouble !== null && (
          // 只有标题是红的。原因和建议各占一行、用正常的字色：三段红字堆在一起，
          // 用户反而分不清哪句是原因、哪句是该做的事。
          <div
            role="alert"
            className="border-destructive/30 bg-destructive/5 border-l-destructive grid gap-1.5 rounded-lg border border-l-4 px-4 py-3 text-sm"
          >
            <p className="text-destructive flex flex-wrap items-center gap-x-2 font-medium">
              <AlertTriangle className="size-4" />
              监控当前不可信
              <span className="text-muted-foreground font-normal">
                恢复之前，表格里的状态不代表门店的真实库存
              </span>
            </p>
            <p className="pl-6">{ui.trouble.reason}</p>
            {ui.trouble.advice !== null && (
              // 用户自己能做的那件事单独拎出来，而且要短：issue #3 里那位对着反复
              // 告警的窗口干等了三个小时，缺的不是解释，是一句「换条网络」。
              <p className="text-muted-foreground pl-6">
                <span className="text-foreground font-medium">建议</span>{" "}
                {describeAdvice(ui.trouble.advice)}
              </p>
            )}
          </div>
        )}

        {ui.update !== null && (
          // 只提示，不自作主张安装。用户可能正等着抢购，被强制重启是灾难。
          <div className="bg-card flex flex-wrap items-center gap-x-4 gap-y-2 rounded-lg border px-4 py-2.5 text-sm shadow-xs">
            <p className="flex items-center gap-2 font-medium">
              <Download className="size-4" /> 有新版本 {ui.update.version}
            </p>
            <p className="text-muted-foreground">
              当前 {ui.update.currentVersion}，安装后需重启应用生效。
            </p>
            <div className="ml-auto flex items-center gap-2">
              <Button size="sm" disabled={ui.installing} onClick={() => void installUpdate()}>
                {ui.installing ? "正在下载…" : "下载并安装"}
              </Button>
              <Button size="sm" variant="ghost" onClick={dismissUpdate}>
                <X /> 稍后
              </Button>
            </div>
          </div>
        )}

        <Panel
          title="账号"
          hint={
            selectedAccount === null
              ? "先新建一个账号；每个账号的监控目标、会话和加购开关互相独立"
              : `${accounts.length} 个账号 · 当前「${selectedAccount.name}」独立监控、独立加购`
          }
          open={accountsOpen}
          onToggle={() => setAccountsOpen(!accountsOpen)}
        >
          <div className="flex flex-wrap items-center gap-2">
            {accounts.map((account) => (
              <button
                key={account.id}
                type="button"
                aria-pressed={account.id === selectedAccount?.id}
                onClick={() => void selectAccount(account.id)}
                className={cn(
                  "flex items-center gap-2 rounded-lg border px-3 py-1.5 text-sm transition-colors",
                  account.id === selectedAccount?.id
                    ? "border-primary bg-accent"
                    : "hover:bg-muted/50",
                )}
              >
                <span
                  className={cn(
                    "size-2 rounded-full",
                    account.running ? "bg-in-stock" : "bg-muted-foreground/40",
                  )}
                  title={account.running ? "监听中" : "已暂停"}
                />
                <span className="font-medium">{account.name}</span>
                <SessionBadge status={account.session.status} label={account.session.statusLabel} />
              </button>
            ))}
            <div className="ml-auto flex flex-wrap items-center gap-2">
              <Input
                aria-label="新账号名称"
                className="h-8 w-36 select-text"
                placeholder="账号名称"
                value={newAccountName}
                onChange={(e) => setNewAccountName(e.target.value)}
              />
              <Input
                aria-label="新账号 Apple ID"
                className="h-8 w-44 select-text"
                placeholder="Apple ID（可选）"
                value={newAccountAppleId}
                onChange={(e) => setNewAccountAppleId(e.target.value)}
              />
              <Button variant="secondary" size="sm" onClick={() => void onCreateAccount()}>
                <UserPlus /> 新建账号
              </Button>
            </div>
          </div>

          {selectedAccount === null ? (
            <p className="text-muted-foreground text-sm">
              还没有账号。新建账号后，才能为它单独添加监控目标、导入浏览器会话并开启自动加购。
            </p>
          ) : (
            <div className="grid gap-3 rounded-lg border p-3">
              <div className="flex flex-wrap items-end gap-3">
                <div className="grid gap-1.5">
                  <Label htmlFor="account-name">账号名称</Label>
                  <Input
                    id="account-name"
                    key={`name-${selectedAccount.id}`}
                    className="w-44 select-text"
                    defaultValue={selectedAccount.name}
                    onBlur={(e) => {
                      const name = e.target.value.trim();
                      if (name !== "" && name !== selectedAccount.name) {
                        patchAccount(selectedAccount, { name });
                      }
                    }}
                  />
                </div>
                <div className="grid gap-1.5">
                  <Label htmlFor="account-apple-id">Apple ID</Label>
                  <Input
                    id="account-apple-id"
                    key={`apple-${selectedAccount.id}`}
                    className="w-56 select-text"
                    placeholder="只用于自己辨认，不会自动登录"
                    defaultValue={selectedAccount.appleId}
                    onBlur={(e) => {
                      const appleId = e.target.value.trim();
                      if (appleId !== selectedAccount.appleId) {
                        patchAccount(selectedAccount, { appleId });
                      }
                    }}
                  />
                </div>
                <div className="grid gap-1.5">
                  <Label>地区</Label>
                  <Combobox
                    className="w-36"
                    options={ui.regions.map((r) => ({ value: r.locale, label: r.title }))}
                    value={selectedAccount.locale}
                    onChange={(locale) => {
                      setStoreNumbers([]);
                      resetProductSelection();
                      void changeLocale(locale);
                    }}
                    placeholder="账号地区"
                    searchPlaceholder="搜索地区…"
                    emptyText="没有匹配的地区"
                  />
                </div>
                <div className="flex items-center gap-2 pb-2">
                  <Switch
                    id="account-enabled"
                    checked={selectedAccount.enabled}
                    onCheckedChange={(enabled) =>
                      patchAccount(selectedAccount, { enabled })
                    }
                  />
                  <Label htmlFor="account-enabled">启用监控</Label>
                </div>
                <div className="flex items-center gap-2 pb-2">
                  <Switch
                    id="account-auto-order"
                    checked={selectedAccount.autoOrder}
                    onCheckedChange={(autoOrder) =>
                      patchAccount(selectedAccount, { autoOrder })
                    }
                  />
                  <Label htmlFor="account-auto-order">有货自动加购</Label>
                </div>
                <Button
                  variant="ghost"
                  size="sm"
                  className="text-destructive hover:text-destructive ml-auto"
                  disabled={accounts.length <= 1}
                  onClick={() => void onDeleteAccount(selectedAccount)}
                >
                  <Trash2 /> 删除账号
                </Button>
              </div>

              <p className="text-muted-foreground text-xs">
                自动加购只把商品放进该账号的 Apple 购物袋，不会替用户付款。付款链接需要用户自己打开并完成支付。
              </p>

              <div className="grid gap-2 rounded-lg border border-dashed p-3">
                <div className="flex flex-wrap items-center gap-2">
                  <KeyRound className="size-4" />
                  <span className="text-sm font-medium">浏览器会话</span>
                  <SessionBadge
                    status={selectedAccount.session.status}
                    label={selectedAccount.session.statusLabel}
                  />
                  <span className="text-muted-foreground text-xs">
                    {selectedAccount.session.configured
                      ? `${selectedAccount.session.cookieCount} 个 Cookie`
                      : "还没有导入 Cookie"}
                  </span>
                  <div className="ml-auto flex flex-wrap items-center gap-2">
                    <Button
                      size="sm"
                      variant="secondary"
                      onClick={() => void openAccountLogin(selectedAccount.id)}
                    >
                      <LogIn /> 登录 Apple
                    </Button>
                    <Button
                      size="sm"
                      variant="outline"
                      onClick={() => void onSaveLoginCookies()}
                    >
                      <KeyRound /> 保存登录 Cookie
                    </Button>
                    <Button
                      size="sm"
                      variant="secondary"
                      disabled={!selectedAccount.session.configured}
                      onClick={() => void checkAccountSession(selectedAccount.id)}
                    >
                      校验会话
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      disabled={!selectedAccount.session.configured}
                      onClick={() => void clearAccountSession(selectedAccount.id)}
                    >
                      清除会话
                    </Button>
                  </div>
                </div>

                {selectedAccount.session.configured && (
                  <p className="text-muted-foreground text-xs">
                    已保存的 Cookie 名称：{selectedAccount.session.cookieNames.join("、")}
                    （原值不回显，也不会写进日志）
                  </p>
                )}

                <textarea
                  aria-label="账号 Cookie"
                  className="border-input placeholder:text-muted-foreground focus-visible:border-ring focus-visible:ring-ring/50 min-h-16 w-full resize-y rounded-md border bg-transparent px-3 py-2 font-mono text-xs outline-none select-text focus-visible:ring-[3px]"
                  placeholder="从准备用来付款的浏览器复制 Cookie，粘贴为 name=value; name2=value2"
                  value={sessionDraft}
                  onChange={(e) => setSessionDraft(e.target.value)}
                />

                <div className="flex flex-wrap items-center gap-2">
                  <Button
                    size="sm"
                    disabled={sessionDraft.trim() === ""}
                    onClick={() => void onSaveSession()}
                  >
                    保存会话
                  </Button>
                  <p className="text-muted-foreground min-w-0 flex-1 text-xs">
                    推荐流程：点「登录 Apple」→ 在官方窗口完成登录 → 回到这里点「保存登录 Cookie」。
                    也可以继续手动粘贴 Cookie。Cookie 会以明文保存在本机账号文件中，属于敏感信息；
                    不要分享该文件，也不要在截图里露出。保存空内容不会清空会话，清空请点「清除会话」。
                  </p>
                </div>
                {selectedAccount.session.detail !== null && (
                  <p className="text-muted-foreground text-xs">
                    上次校验：{selectedAccount.session.detail}
                  </p>
                )}
              </div>
            </div>
          )}
        </Panel>

        <Panel
          title="添加监控目标"
          hint="不同品类、不同门店可以混着加；门店和 iPhone 的机型、容量、颜色都可多选，组合一次全部加入"
        >
          <div className="flex flex-wrap items-end gap-3">
            <div className="grid gap-1.5">
              <Label>地区</Label>
              <Combobox
                className="w-36"
                options={ui.regions.map((r) => ({ value: r.locale, label: r.title }))}
                value={ui.settings.locale}
                onChange={(locale) => {
                  // 换地区后旧的门店和型号都不再适用，清掉待添加的选择。
                  setStoreNumbers([]);
                  resetProductSelection();
                  void changeLocale(locale);
                }}
                placeholder="选择地区"
                searchPlaceholder="搜索地区…"
                emptyText="没有匹配的地区"
              />
            </div>

            <div className="grid gap-1.5">
              <Label>品类</Label>
              <Combobox
                className="w-36"
                options={ui.categories.map((c) => ({ value: c.value, label: c.title }))}
                value={ui.category}
                onChange={(value) => {
                  // 换品类后旧的型号不再在下拉框里，清掉待添加的选择。门店不用清，
                  // 它跟品类无关。
                  resetProductSelection();
                  setCategory(value as Category);
                }}
                placeholder="选择品类"
                searchPlaceholder="搜索品类…"
                emptyText="没有匹配的品类"
                disabled={ui.categories.length === 0}
              />
            </div>

            <div className="grid gap-1.5">
              <Label>门店（可多选）</Label>
              <MultiCombobox
                className="w-64"
                options={storeOptions}
                values={storeNumbers}
                onChange={setStoreNumbers}
                placeholder="选择自提门店"
                searchPlaceholder="搜索门店…"
                emptyText="没有匹配的门店"
                countLabel={(n) => `已选 ${n} 家门店`}
                disabled={storeOptions.length === 0}
              />
            </div>
          </div>

          <div className="flex flex-wrap items-end gap-3">
            {ui.category === "iphone" ? (
              <>
                <div className="grid gap-1.5">
                  <Label>机型（可多选）</Label>
                  <MultiCombobox
                    className="w-48"
                    options={iphoneFamilyOptions}
                    values={families}
                    onChange={onFamiliesChange}
                    placeholder="选择机型"
                    searchPlaceholder="搜索机型…"
                    emptyText="没有匹配的机型"
                    countLabel={(n) => `已选 ${n} 款机型`}
                    disabled={iphoneFamilyOptions.length === 0}
                  />
                </div>

                <div className="grid gap-1.5">
                  <Label>容量（可多选）</Label>
                  <MultiCombobox
                    className="w-36"
                    options={iphoneCapacityOptions}
                    values={capacities}
                    onChange={onCapacitiesChange}
                    placeholder="选择容量"
                    searchPlaceholder="搜索容量…"
                    emptyText="没有匹配的容量"
                    countLabel={(n) => `已选 ${n} 种容量`}
                    disabled={families.length === 0 || iphoneCapacityOptions.length === 0}
                  />
                </div>

                <div className="grid gap-1.5">
                  <Label>颜色（可多选）</Label>
                  <MultiCombobox
                    className="w-40"
                    options={iphoneColorOptions}
                    values={colors}
                    onChange={setColors}
                    placeholder="选择颜色"
                    searchPlaceholder="搜索颜色…"
                    emptyText="没有匹配的颜色"
                    countLabel={(n) => `已选 ${n} 种颜色`}
                    disabled={capacities.length === 0 || iphoneColorOptions.length === 0}
                  />
                </div>
              </>
            ) : (
              <div className="grid gap-1.5">
                <Label>型号</Label>
                <Combobox
                  className="w-96"
                  options={productOptions}
                  value={partNumber}
                  onChange={setPartNumber}
                  placeholder="选择型号"
                  searchPlaceholder="搜索型号…"
                  emptyText="没有匹配的型号"
                  disabled={productOptions.length === 0}
                />
              </div>
            )}

            <Button variant="secondary" onClick={() => void onAdd()} disabled={!canAdd}>
              <Plus /> 添加
            </Button>
            {selectedProducts.length > 1 && (
              // 多选时把要加的规模点明：三级各勾几项之后，组合数并不直观。
              <span
                className="text-muted-foreground pb-2 text-sm"
                title={selectedProducts.map((p) => p.title).join("、")}
              >
                {selectedProducts.length} 个型号 × {storeNumbers.length} 家门店
              </span>
            )}

            <Tooltip>
              <TooltipTrigger asChild>
                <Button
                  variant="ghost"
                  size="icon"
                  aria-label="从 Apple 官网更新当前品类的型号列表"
                  disabled={ui.refreshing}
                  onClick={() => void refreshProducts()}
                >
                  <RefreshCw className={ui.refreshing ? "animate-spin" : undefined} />
                </Button>
              </TooltipTrigger>
              <TooltipContent>
                从 Apple 官网更新当前品类的型号列表。新机发布后用这个，不必等程序更新。
              </TooltipContent>
            </Tooltip>
          </div>
        </Panel>

        <Panel
          title="运行设置"
          hint={settingsOpen ? "改动即时保存" : settingsSummary(ui.settings)}
          open={settingsOpen}
          onToggle={toggleSettings}
        >
          {/* 两栏：左边是查询与本机提醒，右边是推送地址。推送地址一行一个会占好几行，
              和别的设置叠成一列的话，默认窗口高度下监控列表会被挤得看不见。 */}
          <div className="flex flex-wrap items-start gap-x-8 gap-y-4">
            <div className="grid min-w-80 grow-[2] basis-80 gap-3">
              <div className="grid gap-1.5">
                {/* 标题行和右栏「推送地址」一样高，两栏的输入框才能对齐。 */}
                <div className="flex h-3.5 items-center gap-x-2">
                  <Label htmlFor="proxy-0" className="shrink-0 whitespace-nowrap">
                    代理地址（可选）
                  </Label>
                  <span className="text-muted-foreground min-w-0 truncate text-xs leading-none">
                    http / https / socks5，一行一个
                  </span>
                  {lastProxyRow.trim() !== "" && (
                    <Button
                      variant="link"
                      size="sm"
                      className="text-muted-foreground hover:text-foreground ml-auto h-3.5 shrink-0 gap-1 p-0 text-xs leading-none has-[>svg]:px-0 [&_svg]:size-3"
                      onClick={() => setProxyDraft([...cleanPushRows(visibleProxyRows), ""])}
                    >
                      <Plus /> 添加代理
                    </Button>
                  )}
                </div>
                {visibleProxyRows.map((row, index) => {
                  // 保存后后端没留下这一行：地址解析不了，这条线路不存在。
                  const rejected =
                    proxyDraft !== null && row.trim() !== "" && !savedProxies.includes(row.trim());
                  return (
                    <div key={index} className="flex items-center gap-2">
                      <Input
                        id={`proxy-${index}`}
                        aria-label={`代理地址 ${index + 1}`}
                        aria-invalid={rejected || undefined}
                        className="select-text"
                        placeholder="socks5://127.0.0.1:1080 或 http://user:pass@host:port"
                        value={row}
                        autoFocus={proxyDraft !== null && row === "" && index > 0}
                        onChange={(e) => editProxyRow(index, e.target.value)}
                        onBlur={() => commitProxyRows(visibleProxyRows)}
                      />
                      {rejected && (
                        <Badge
                          variant="outline"
                          className="text-destructive border-destructive/40 shrink-0"
                          title="地址格式不对，这条线路没有生效。支持 http、https、socks5、socks5h，例如 socks5://127.0.0.1:1080"
                        >
                          未生效
                        </Badge>
                      )}
                      <Button
                        variant="ghost"
                        size="icon"
                        aria-label={`删除代理地址 ${index + 1}`}
                        disabled={visibleProxyRows.length === 1 && row === ""}
                        onClick={() =>
                          commitProxyRows(visibleProxyRows.filter((_, i) => i !== index))
                        }
                      >
                        <Trash2 />
                      </Button>
                    </div>
                  );
                })}
              </div>

              <div className="flex flex-wrap items-center gap-x-5 gap-y-2">
                <div className="flex items-center gap-2">
                  <Label htmlFor="interval" className="whitespace-nowrap">
                    查询间隔
                  </Label>
                  <Input
                    id="interval"
                    type="number"
                    min={5}
                    className="h-8 w-20 select-text"
                    value={intervalValue}
                    onChange={(e) => setIntervalDraft(e.target.valueAsNumber)}
                    onBlur={() => {
                      const s = Number.isFinite(intervalValue) ? Math.round(intervalValue) : 30;
                      setIntervalDraft(null);
                      void setIntervalSeconds(s);
                    }}
                  />
                  <span className="text-muted-foreground text-sm">秒</span>
                </div>
                <div className="flex items-center gap-2">
                  <Switch
                    id="sound"
                    checked={ui.settings.soundEnabled}
                    onCheckedChange={(v) =>
                      void saveSettings({ ...ui.settings, soundEnabled: v })
                    }
                  />
                  <Label htmlFor="sound">提示音</Label>
                </div>
                <div className="flex items-center gap-2">
                  <Switch
                    id="openbag"
                    checked={ui.settings.openBagOnHit}
                    onCheckedChange={(v) =>
                      void saveSettings({ ...ui.settings, openBagOnHit: v })
                    }
                  />
                  <Label htmlFor="openbag">有货时打开购物袋</Label>
                  <span className="text-muted-foreground text-xs">
                    自动加购的账号命中时不打开
                  </span>
                </div>
              </div>
            </div>

            <div className="grid min-w-96 grow-[3] basis-96 gap-1.5">
              {/* 标题行和左栏的标签一样高，两栏的第一行输入框才能对齐。 */}
              <div className="flex h-3.5 items-center gap-x-2">
                <Label htmlFor="push-0" className="shrink-0 whitespace-nowrap">
                  推送地址（可选）
                </Label>
                <span className="text-muted-foreground min-w-0 truncate text-xs leading-none">
                  Bark 或飞书群机器人，一行一个，自动识别
                </span>
                {lastPushRow.trim() !== "" && (
                  <Button
                    variant="link"
                    size="sm"
                    className="text-muted-foreground hover:text-foreground ml-auto h-3.5 shrink-0 gap-1 p-0 text-xs leading-none has-[>svg]:px-0 [&_svg]:size-3"
                    onClick={() => setPushDraft([...cleanPushRows(visiblePushRows), ""])}
                  >
                    <Plus /> 添加推送地址
                  </Button>
                )}
                {/* 测试的就是这些推送（连同系统通知和提示音），放在它们的标题旁边。 */}
                <Button
                  variant="link"
                  size="sm"
                  className={`text-muted-foreground hover:text-foreground h-3.5 shrink-0 gap-1 p-0 text-xs leading-none has-[>svg]:px-0 [&_svg]:size-3 ${lastPushRow.trim() !== "" ? "" : "ml-auto"}`}
                  onClick={() => void testNotify()}
                >
                  <BellRing /> 测试提醒
                </Button>
              </div>
              {visiblePushRows.map((row, index) => {
                const kind = savedPushKind.get(row.trim());
                return (
                  <div key={index} className="flex items-center gap-2">
                    {/* 渠道标签占固定宽度：还没保存的行留空，输入框照样对齐。 */}
                    <span className="flex w-12 shrink-0 justify-center">
                      {kind !== undefined && (
                        <Badge variant="outline" className="text-muted-foreground">
                          {PUSH_KIND_LABEL[kind]}
                        </Badge>
                      )}
                    </span>
                    <Input
                      id={`push-${index}`}
                      aria-label={`推送地址 ${index + 1}`}
                      className="select-text"
                      placeholder="粘贴 Bark 地址，或飞书群机器人的 webhook 地址"
                      value={row}
                      // 只有点「添加」新开的那一行自动聚焦；启动时的空行不抢焦点。
                      autoFocus={pushDraft !== null && row === "" && index > 0}
                      onChange={(e) => editPushRow(index, e.target.value)}
                      onBlur={() => commitPushRows(visiblePushRows)}
                    />
                    <Button
                      variant="ghost"
                      size="icon"
                      aria-label={`删除推送地址 ${index + 1}`}
                      disabled={visiblePushRows.length === 1 && row === ""}
                      onClick={() =>
                        commitPushRows(visiblePushRows.filter((_, i) => i !== index))
                      }
                    >
                      <Trash2 />
                    </Button>
                  </div>
                );
              })}
              {savedPush.some((p) => p.kind === "feishu") && (
                // 飞书机器人最常见的配置错误是安全设置，只在真填了飞书地址时提醒。
                <p className="text-muted-foreground pl-14 text-xs">
                  飞书机器人的安全设置请选「自定义关键词」，填「有货」。
                </p>
              )}
            </div>
          </div>
        </Panel>

          </div>

          <div className="flex min-h-0 flex-col gap-3">
        <section className="bg-card flex min-h-[360px] flex-1 flex-col overflow-hidden rounded-xl border shadow-xs">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-2 border-b px-4 py-2.5">
            <h2 className="shrink-0 text-sm font-medium">监控列表</h2>
            <span className="text-muted-foreground shrink-0 text-xs">
              {selectedAccount?.name ?? "未选择账号"} · 每行一条独立监控
            </span>
            <div className="ml-auto flex flex-wrap items-center gap-1.5">
              <FilterChip
                label="全部"
                count={ui.rows.length}
                active={statusFilter === "all"}
                onClick={() => setStatusFilter("all")}
              />
              <FilterChip
                label="有货"
                count={summary.inStock}
                tone="inStock"
                active={statusFilter === "in_stock"}
                onClick={() => setStatusFilter("in_stock")}
              />
              <FilterChip
                label="无货"
                count={summary.outOfStock}
                active={statusFilter === "out_of_stock"}
                onClick={() => setStatusFilter("out_of_stock")}
              />
              {summary.untrusted > 0 && (
                // 「其中多少项查不到」单独点出来：这个数字大于 0 时，
                // 列表里那些「无货」也未必反映真实情况。
                <FilterChip
                  label="查不到"
                  count={summary.untrusted}
                  tone="unknown"
                  active={statusFilter === "unknown"}
                  onClick={() => setStatusFilter("unknown")}
                />
              )}
            </div>
          </div>
          {ui.running && ui.pacing !== null && ui.pacing.paced && (
            // 请求预算在拉长间隔时明说，否则用户会以为设的 30 秒没生效。
            <p className="text-muted-foreground border-b px-4 py-1.5 text-xs">
              受 Apple 频率限制，下一轮 {ui.pacing.nextCheckInSecs} 秒后
            </p>
          )}
          <ScrollArea className="min-h-0 flex-1">
            <Table>
              <TableHeader>
                <TableRow className="hover:bg-transparent">
                  <TableHead className="text-muted-foreground w-24 pl-4 text-xs">状态</TableHead>
                  <TableHead className="text-muted-foreground text-xs">门店</TableHead>
                  <TableHead className="text-muted-foreground text-xs">型号</TableHead>
                  <TableHead className="text-muted-foreground w-24 text-xs">最后检查</TableHead>
                  <TableHead className="w-24" />
                </TableRow>
              </TableHeader>
              <TableBody>
                {visibleRows.length === 0 ? (
                  <TableRow className="hover:bg-transparent">
                    <TableCell colSpan={5} className="text-muted-foreground h-28 text-center">
                      {ui.rows.length === 0
                        ? ui.ready
                          ? "还没有监控目标。在上面选好门店和型号，点「添加」。"
                          : "正在载入…"
                        : "当前筛选下没有监控项。"}
                    </TableCell>
                  </TableRow>
                ) : (
                  visibleRows.map((row) => (
                    <TableRow
                      key={targetKey(row.target)}
                      // 有货的行整行铺一层有货色。这一刻是这个程序存在的全部理由，
                      // 不该只靠一枚小徽标。
                      className={cn(
                        "odd:bg-muted/30",
                        row.availability.kind === "in_stock" &&
                          "bg-in-stock/10 odd:bg-in-stock/10 hover:bg-in-stock/15",
                      )}
                    >
                      <TableCell className="pl-4">
                        <StatusBadge availability={row.availability} />
                      </TableCell>
                      <TableCell className="font-medium">{row.target.storeTitle}</TableCell>
                      <TableCell className="text-muted-foreground">
                        {row.target.productName}
                      </TableCell>
                      <TableCell className="text-muted-foreground tabular-nums">
                        {formatTime(row.lastCheckedMs)}
                      </TableCell>
                      <TableCell>
                        {row.availability.kind === "in_stock" && selectedAccount !== null && (
                          <Button
                            variant="ghost"
                            size="icon"
                            aria-label="把这条加入当前账号购物袋"
                            title={
                              selectedAccount.session.configured
                                ? "加入当前账号的 Apple 购物袋"
                                : "先在账号面板导入会话"
                            }
                            disabled={!selectedAccount.session.configured}
                            onClick={() => void placeOrder(selectedAccount.id, row.target)}
                          >
                            <CreditCard />
                          </Button>
                        )}
                        <Button
                          variant="ghost"
                          size="icon"
                          aria-label="删除这条监控"
                          onClick={() => void onRemove(row.target)}
                        >
                          <Trash2 />
                        </Button>
                      </TableCell>
                    </TableRow>
                  ))
                )}
              </TableBody>
            </Table>
          </ScrollArea>
        </section>

        <section className="bg-card h-28 shrink-0 overflow-hidden rounded-xl border shadow-xs">
          <ScrollArea className="h-full">
            <div className="p-3 font-mono text-xs leading-5 select-text">
              {ui.logs.length === 0 ? (
                <p className="text-muted-foreground">日志会显示在这里。</p>
              ) : (
                ui.logs.map((line, i) => <LogLine key={i} line={line} />)
              )}
            </div>
          </ScrollArea>
        </section>
          </div>
        </div>
      </div>
    </TooltipProvider>
  );
}
