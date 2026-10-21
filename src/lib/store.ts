/**
 * 界面状态的唯一入口。
 *
 * 有一条铁律：**前端不持有真源**。这里存的每一样东西要么是 Rust 推过来的原样
 * 副本，要么是纯粹的展示状态（比如日志文本）。绝不在前端自己判断「这个型号
 * 到底有没有货」—— 一旦前端有了自己的一份判断，那条花大力气在 Rust 里守住的
 * 不变量就会从前门溜回来。
 *
 * 用 `useSyncExternalStore` 而不是 Context 或状态库：它就是为「订阅外部数据源」
 * 设计的，而我们的数据源正是 Rust。再套一层状态库只会制造「再存一份」的诱惑。
 */

import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { openUrl } from "@tauri-apps/plugin-opener";

import type {
  Account,
  AccountEvent,
  AccountPatch,
  AccountOrderEvent,
  Category,
  CategoryOption,
  OrderRecord,
  Product,
  Region,
  Settings,
  Store,
  Target,
  TargetState,
  Trouble,
  UpdateInfo,
} from "./types";
import { assertNever, describeAvailability, isUntrusted } from "./types";

const EVENT_CHANNEL = "watcher://event";
const ORDER_CHANNEL = "watcher://order";
const NOTICE_CHANNEL = "watcher://notice";
const MAX_LOG_LINES = 300;

export interface UiState {
  /** 所有账号。界面用 id 路由快照和订单。 */
  accounts: Account[];
  /** 当前选中的账号；null 表示还没有账号。 */
  selectedAccountId: string | null;
  /** 每个账号最近一轮的完整快照。`rows` 是选中账号的那一份副本。 */
  accountRows: Record<string, TargetState[]>;
  /** 每个账号的运行态。`running` 是选中账号的那一份副本。 */
  accountRunning: Record<string, boolean>;
  rows: TargetState[];
  running: boolean;
  /** 非 null 表示「当前的状态不可信」，界面要挂一条持续可见的告警。 */
  trouble: Trouble | null;
  /**
   * 引擎上一轮结束时报的下一轮时刻。`paced` 为 true 表示是请求预算在拉长
   * 等待（Apple 按出口 IP 限制取货查询的频率），而不是用户设的间隔。
   */
  pacing: { nextCheckInSecs: number; paced: boolean } | null;
  logs: string[];
  regions: Region[];
  categories: CategoryOption[];
  stores: Store[];
  products: Product[];
  /**
   * 当前正在挑选的品类。
   *
   * 只是个筛选器，所以不进设置、不落盘：它决定型号下拉框里显示哪一批商品，
   * 以及刷新按钮去抓哪几页。已经加进监控列表的目标不受它影响 —— 列表里
   * 四个品类是混在一起的，不然用户切一下品类就以为自己的监控项没了。
   */
  category: Category;
  settings: Settings;
  /** 正在从 Apple 官网刷新型号列表。 */
  refreshing: boolean;
  ready: boolean;
  /** 检查到的新版本；null 表示已是最新或还没查。 */
  update: UpdateInfo | null;
  /** 正在下载安装更新。 */
  installing: boolean;
  /** 每个账号最近一次加购结果。 */
  orders: Record<string, OrderRecord | null>;
}

const DEFAULT_SETTINGS: Settings = {
  locale: "zh_CN",
  targets: [],
  intervalSeconds: 30,
  barkUrl: "",
  feishuWebhook: "",
  soundEnabled: true,
  openBagOnHit: true,
  proxies: [],
};

let state: UiState = {
  accounts: [],
  selectedAccountId: null,
  accountRows: {},
  accountRunning: {},
  orders: {},
  rows: [],
  running: false,
  trouble: null,
  pacing: null,
  logs: [],
  regions: [],
  categories: [],
  stores: [],
  products: [],
  category: "iphone",
  settings: DEFAULT_SETTINGS,
  refreshing: false,
  ready: false,
  update: null,
  installing: false,
};

const listeners = new Set<() => void>();

/**
 * 返回缓存的引用。
 *
 * `useSyncExternalStore` 用 `Object.is` 比较，每次返回新对象会导致无限重渲染，
 * 所以只在真正变更时才替换 `state`。
 */
function getSnapshot(): UiState {
  return state;
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

function update(patch: Partial<UiState>): void {
  state = { ...state, ...patch };
  for (const cb of listeners) cb();
}

function pushLog(line: string): void {
  const stamp = new Date().toLocaleTimeString("zh-CN", { hour12: false });
  const next = [...state.logs, `[${stamp}] ${line}`];
  // 定长保留。上游把日志无限拼进一个字符串，跑一整天能有几 MB，
  // 每次刷新都要重新排版，界面越用越卡。
  update({ logs: next.length > MAX_LOG_LINES ? next.slice(-MAX_LOG_LINES) : next });
}

function accountName(id: string): string {
  return (state.accounts ?? []).find((account) => account.id === id)?.name ?? "账号";
}

function replaceAccountRows(accountId: string, rows: TargetState[]): Record<string, TargetState[]> {
  return { ...(state.accountRows ?? {}), [accountId]: rows };
}

function applyEvent(payload: AccountEvent): void {
  const { accountId, event } = payload;
  const selected = state.selectedAccountId === accountId;
  const name = accountName(accountId);

  switch (event.type) {
    case "stateChanged": {
      // 列表本身以 cycleComplete 带来的快照为准，不拿这条事件去增量改 ——
      // 它是可丢弃的，用它做增量会让界面和引擎慢慢对不上。
      // 这里只做一件事：把「某一行开始不可信」记进日志，附上具体原因，
      // 否则用户只能看到一个「未知」，不知道到底出了什么事。
      const { availability, target } = event.state;
      if (isUntrusted(availability)) {
        const { detail } = describeAvailability(availability);
        pushLog(`${name} · ${target.storeTitle} ${target.productName}：${detail ?? "查询失败"}`);
      }
      break;
    }

    case "inStock":
      pushLog(
        `${name} 有货！${event.state.target.storeTitle} ${event.state.target.productName}`,
      );
      break;

    case "cycleComplete": {
      const accountRows = replaceAccountRows(accountId, event.snapshot);
      if (!selected) {
        update({ accountRows });
        break;
      }
      const recovered = event.healthy && state.trouble !== null;
      const pacedBefore = state.pacing?.paced ?? false;
      update({
        accountRows,
        rows: event.snapshot,
        // 只有引擎明说本轮健康，才收起告警。用「所有行都没错误」去反推是
        // 不可靠的：某些故障路径下状态压根没被更新。
        trouble: event.healthy ? null : state.trouble,
        pacing: { nextCheckInSecs: event.nextCheckInSecs, paced: event.paced },
      });
      if (recovered) pushLog("查询已恢复正常。");
      // 只在开始被拉长的那一轮记一条，之后每轮都写会把日志刷满。
      if (event.paced && !pacedBefore) {
        pushLog(
          `Apple 限制每个网络的查询频率，本轮起按请求预算放慢：下一轮 ${event.nextCheckInSecs} 秒后。`,
        );
      }
      break;
    }

    case "trouble":
      if (selected) {
        update({ trouble: { reason: event.reason, advice: event.advice } });
      }
      pushLog(`${name} 告警：${event.reason}`);
      break;

    case "runStateChanged": {
      const accountRunning = { ...(state.accountRunning ?? {}), [accountId]: event.running };
      const accounts = (state.accounts ?? []).map((account) =>
        account.id === accountId ? { ...account, running: event.running } : account,
      );
      update({
        accountRunning,
        accounts,
        ...(selected
          ? {
              running: event.running,
              pacing: event.running ? state.pacing : null,
            }
          : {}),
      });
      pushLog(`${name}${event.running ? "已开始监控。" : "已暂停监控。"}`);
      break;
    }

    default:
      assertNever(event);
  }
}

function applyOrderEvent({ accountId, order }: AccountOrderEvent): void {
  const name = accountName(accountId);
  const orders = { ...(state.orders ?? {}), [accountId]: order };
  const accounts = (state.accounts ?? []).map((account) =>
    account.id === accountId ? { ...account, lastOrder: order } : account,
  );
  update({ orders, accounts });
  if (order.status === "added") {
    pushLog(
      `${name} 已把 ${order.productName} 加入购物袋，请打开付款链接完成付款。`,
    );
  } else {
    pushLog(`${name} 自动加购失败：${order.message}`);
  }
}

let unlisteners: UnlistenFn[] = [];
let starting: Promise<void> | null = null;

/**
 * 连接后端。重复调用是安全的。
 *
 * React 开发模式下 StrictMode 会把 effect 执行两次，如果每次都注册监听器，
 * 会得到两份 —— 日志每行打两遍、提醒重复触发。这个问题只在 dev 复现、生产不会，
 * 最容易漏到很后面才发现，所以这里用一个进行中的 Promise 做去重。
 */
export function connect(): Promise<void> {
  if (starting) return starting;
  starting = (async () => {
    unlisteners = await Promise.all([
      listen<AccountEvent>(EVENT_CHANNEL, (e) => applyEvent(e.payload)),
      listen<AccountOrderEvent>(ORDER_CHANNEL, (e) => applyOrderEvent(e.payload)),
      // 这条通道不只在启动时用：发提醒失败、打开购物袋失败也走这里，前缀不能写死成「启动」。
      listen<string>(NOTICE_CHANNEL, (e) => pushLog(`提示：${e.payload}`)),
    ]);

    const [regions, categories, settings, accounts] = await Promise.all([
      invoke<Region[]>("list_regions"),
      invoke<CategoryOption[]>("list_categories"),
      invoke<Settings>("get_settings"),
      invoke<Account[]>("list_accounts"),
    ]);
    const selected = accounts[0] ?? null;
    const rows = selected
      ? await invoke<TargetState[]>("get_account_snapshot", { accountId: selected.id })
      : [];
    const locale = selected?.locale ?? settings.locale;
    const accountRows = selected ? { [selected.id]: rows } : {};
    const accountRunning = Object.fromEntries(
      accounts.map((account) => [account.id, account.running]),
    );
    const orders = Object.fromEntries(
      accounts.map((account) => [account.id, account.lastOrder]),
    );
    update({
      regions,
      categories,
      settings: {
        ...settings,
        locale,
        intervalSeconds: selected?.intervalSeconds ?? settings.intervalSeconds,
      },
      accounts,
      selectedAccountId: selected?.id ?? null,
      accountRows,
      accountRunning,
      orders,
      rows,
      running: selected?.running ?? false,
      ready: true,
    });
    await loadCatalog(locale);
    // 启动时静默查一次。查不到就算了，不打扰用户 —— 网络不通、GitHub 抽风
    // 都会走到这里，跟「有没有新版本」是两回事。
    void checkForUpdate({ quiet: true });
  })();
  return starting;
}

export function disconnect(): void {
  for (const un of unlisteners) un();
  unlisteners = [];
  starting = null;
}

export const watcherStore = { subscribe, getSnapshot };

// ---- 命令。全部只是转发，不含任何业务判断。

let catalogRequest = 0;

/** 载入某地区的门店与型号目录。 */
export async function loadCatalog(locale: string): Promise<void> {
  // 官网刷新可能在用户切换地区后才完成，不再为旧地区发起目录请求。
  if (locale !== state.settings.locale) return;
  const request = ++catalogRequest;
  const isCurrent = () => request === catalogRequest && locale === state.settings.locale;
  try {
    const [stores, products] = await Promise.all([
      invoke<Store[]>("list_stores", { locale }),
      invoke<Product[]>("list_products", { locale }),
    ]);
    if (isCurrent()) update({ stores, products });
  } catch (err) {
    if (!isCurrent()) return;
    // 目录读不出来不该让整个界面挂掉，但必须让用户知道下拉为什么是空的。
    update({ stores: [], products: [] });
    pushLog(`载入 ${locale} 的门店与型号失败：${String(err)}`);
  }
}

export async function saveSettings(next: Settings): Promise<void> {
  try {
    const saved = await invoke<Settings>("save_settings", { settings: next });
    if (saved.locale !== state.settings.locale) {
      // 新地区目录返回之前不能沿用旧门店/型号，否则会添加跨区监控目标。
      catalogRequest += 1;
      update({ settings: saved, stores: [], products: [] });
      await loadCatalog(saved.locale);
    } else {
      update({ settings: saved });
    }
  } catch (err) {
    pushLog(`保存设置失败：${String(err)}`);
  }
}

/**
 * 用界面上的推送地址列表整体替换现有的。
 *
 * 每个地址是 Bark 还是飞书由后端按地址长相判断并分栏存好，这里原样交过去，
 * 拿回存好的设置；界面上的渠道标签就按存回来的位置显示。
 */
export async function setPushUrls(urls: string[]): Promise<void> {
  try {
    const saved = await invoke<Settings>("set_push_urls", { urls });
    update({ settings: saved });
  } catch (err) {
    pushLog(`保存推送地址失败：${String(err)}`);
  }
}

export function setCategory(category: Category): void {
  update({ category });
}

export async function changeLocale(locale: string): Promise<void> {
  const accountId = state.selectedAccountId;
  if (accountId) {
    try {
      const saved = await invoke<Account>("set_account_locale", { accountId, locale });
      catalogRequest += 1;
      update({
        settings: { ...state.settings, locale: saved.locale },
        accounts: (state.accounts ?? []).map((account) =>
          account.id === accountId ? saved : account,
        ),
        stores: [],
        products: [],
      });
      await loadCatalog(saved.locale);
    } catch (err) {
      pushLog(`保存账号地区失败：${String(err)}`);
    }
    return;
  }
  await saveSettings({ ...state.settings, locale });
}

export async function setTargets(targets: Target[]): Promise<void> {
  const accountId = state.selectedAccountId;
  try {
    if (accountId) {
      const rows = await invoke<TargetState[]>("set_account_targets", { accountId, targets });
      update({
        rows,
        accountRows: replaceAccountRows(accountId, rows),
        accounts: (state.accounts ?? []).map((account) =>
          account.id === accountId ? { ...account, targetCount: targets.length } : account,
        ),
      });
      return;
    }
    const rows = await invoke<TargetState[]>("set_targets", { targets });
    update({ rows, settings: { ...state.settings, targets } });
  } catch (err) {
    pushLog(`更新监控列表失败：${String(err)}`);
  }
}

export async function startWatching(): Promise<void> {
  const accountId = state.selectedAccountId;
  try {
    if (accountId) {
      await invoke("start_account", { accountId });
      return;
    }
    await invoke("start_watching");
  } catch (err) {
    pushLog(`开始监控失败：${String(err)}`);
  }
}

export async function stopWatching(): Promise<void> {
  const accountId = state.selectedAccountId;
  try {
    if (accountId) {
      await invoke("stop_account", { accountId });
      return;
    }
    await invoke("stop_watching");
  } catch (err) {
    pushLog(`暂停监控失败：${String(err)}`);
  }
}

export async function setIntervalSeconds(seconds: number): Promise<void> {
  const accountId = state.selectedAccountId;
  try {
    if (accountId) {
      const applied = await invoke<number>("set_account_interval", { accountId, seconds });
      update({
        accounts: (state.accounts ?? []).map((account) =>
          account.id === accountId ? { ...account, intervalSeconds: applied } : account,
        ),
        settings: { ...state.settings, intervalSeconds: applied },
      });
      return;
    }
    const applied = await invoke<number>("set_interval", { seconds });
    update({ settings: { ...state.settings, intervalSeconds: applied } });
  } catch (err) {
    pushLog(`设置查询间隔失败：${String(err)}`);
  }
}

// ---- 账号管理。所有函数都只是转发命令，业务判断留在 Rust。

function replaceAccount(account: Account): Account[] {
  return (state.accounts ?? []).map((item) => (item.id === account.id ? account : item));
}

export async function selectAccount(accountId: string): Promise<void> {
  const account = (state.accounts ?? []).find((item) => item.id === accountId);
  if (!account) return;
  try {
    const rows = await invoke<TargetState[]>("get_account_snapshot", { accountId });
    catalogRequest += 1;
    update({
      selectedAccountId: accountId,
      rows,
      accountRows: replaceAccountRows(accountId, rows),
      running: (state.accountRunning ?? {})[accountId] ?? account.running,
      settings: {
        ...state.settings,
        locale: account.locale,
        intervalSeconds: account.intervalSeconds,
      },
      // 告警和节奏属于账号，切换时先清掉上一个账号的展示。
      trouble: null,
      pacing: null,
    });
    await loadCatalog(account.locale);
  } catch (err) {
    pushLog(`切换账号失败：${String(err)}`);
  }
}

export async function refreshAccounts(): Promise<Account[]> {
  const accounts = await invoke<Account[]>("list_accounts");
  const selectedId =
    state.selectedAccountId && accounts.some((account) => account.id === state.selectedAccountId)
      ? state.selectedAccountId
      : (accounts[0]?.id ?? null);
  update({ accounts, selectedAccountId: selectedId });
  return accounts;
}

export async function addAccount(
  name: string,
  appleId: string,
  locale: string,
): Promise<Account | null> {
  try {
    const account = await invoke<Account>("create_account", { name, appleId, locale });
    update({ accounts: [...(state.accounts ?? []), account] });
    await selectAccount(account.id);
    pushLog(`已添加账号「${account.name}」。`);
    return account;
  } catch (err) {
    pushLog(`添加账号失败：${String(err)}`);
    return null;
  }
}

export async function updateAccount(
  accountId: string,
  patch: AccountPatch,
): Promise<Account | null> {
  try {
    const account = await invoke<Account>("update_account", { accountId, patch });
    update({ accounts: replaceAccount(account) });
    if (account.id === state.selectedAccountId) {
      update({
        running: account.running,
        settings: { ...state.settings, locale: account.locale, intervalSeconds: account.intervalSeconds },
      });
    }
    return account;
  } catch (err) {
    pushLog(`更新账号失败：${String(err)}`);
    return null;
  }
}

export async function deleteAccount(accountId: string): Promise<void> {
  try {
    await invoke("delete_account", { accountId });
    const accounts = (state.accounts ?? []).filter((account) => account.id !== accountId);
    const accountRows = { ...(state.accountRows ?? {}) };
    delete accountRows[accountId];
    const selectedAccountId =
      state.selectedAccountId === accountId
        ? (accounts[0]?.id ?? null)
        : state.selectedAccountId;
    update({ accounts, accountRows, selectedAccountId });
    if (selectedAccountId) {
      await selectAccount(selectedAccountId);
    } else {
      update({ rows: [], running: false, trouble: null, pacing: null });
    }
    pushLog("账号已删除，本地会话也随账号文件一并移除。");
  } catch (err) {
    pushLog(`删除账号失败：${String(err)}`);
  }
}

export async function saveAccountSession(
  accountId: string,
  cookies: string,
): Promise<Account | null> {
  try {
    const account = await invoke<Account>("set_account_session", { accountId, cookies });
    update({ accounts: replaceAccount(account) });
    pushLog("账号会话已保存在本地；不会上传，也不会在日志里显示。");
    return account;
  } catch (err) {
    pushLog(`保存账号会话失败：${String(err)}`);
    return null;
  }
}

export async function openAccountLogin(accountId: string): Promise<void> {
  try {
    await invoke("open_account_login", { accountId });
    pushLog("已打开 Apple 官方登录窗口。完成登录后回到这里点击「保存登录 Cookie」。");
  } catch (err) {
    pushLog(`打开 Apple 登录窗口失败：${String(err)}`);
  }
}

export async function saveAccountLoginCookies(accountId: string): Promise<Account | null> {
  try {
    const account = await invoke<Account>("save_account_login_cookies", { accountId });
    update({ accounts: replaceAccount(account) });
    pushLog("已从登录窗口读取并保存 Apple Cookie；现在可以校验会话。");
    return account;
  } catch (err) {
    pushLog(`保存登录 Cookie 失败：${String(err)}`);
    return null;
  }
}

export async function clearAccountSession(accountId: string): Promise<Account | null> {
  try {
    const account = await invoke<Account>("clear_account_session", { accountId });
    update({ accounts: replaceAccount(account) });
    pushLog("账号会话已清除。");
    return account;
  } catch (err) {
    pushLog(`清除账号会话失败：${String(err)}`);
    return null;
  }
}

export async function checkAccountSession(accountId: string): Promise<Account | null> {
  try {
    const account = await invoke<Account>("check_account_session", { accountId });
    update({ accounts: replaceAccount(account) });
    pushLog(`会话校验：${account.session.statusLabel}`);
    return account;
  } catch (err) {
    pushLog(`校验账号会话失败：${String(err)}`);
    return null;
  }
}

export async function startAllAccounts(): Promise<void> {
  try {
    await invoke("start_all");
  } catch (err) {
    pushLog(`启动全部账号失败：${String(err)}`);
  }
}

export async function stopAllAccounts(): Promise<void> {
  try {
    await invoke("stop_all");
  } catch (err) {
    pushLog(`暂停全部账号失败：${String(err)}`);
  }
}

/**
 * 手动加购。成功后后端会通过 `watcher://order` 把结果广播回来；
 * 这里只做一次乐观更新，避免界面等事件到达才显示付款链接。
 */
export async function placeOrder(accountId: string, target?: Target): Promise<OrderRecord | null> {
  try {
    const order = await invoke<OrderRecord>("place_order_now", {
      accountId,
      target: target ?? null,
    });
    const orders = { ...(state.orders ?? {}), [accountId]: order };
    const accounts = (state.accounts ?? []).map((account) =>
      account.id === accountId ? { ...account, lastOrder: order } : account,
    );
    update({ orders, accounts });
    return order;
  } catch (err) {
    pushLog(`加购失败：${String(err)}`);
    return null;
  }
}

/** 用系统浏览器打开付款链接。链接与会话绑定，必须由用户自己完成付款。 */
export async function openPaymentLink(url: string): Promise<void> {
  if (!url.trim()) return;
  try {
    await openUrl(url);
  } catch (err) {
    pushLog(`打开付款链接失败：${String(err)}`);
  }
}

/** 复制付款链接。剪贴板不可用时明确报错，不假装复制成功。 */
export async function copyPaymentLink(url: string): Promise<void> {
  if (!url.trim()) return;
  try {
    await navigator.clipboard.writeText(url);
    pushLog("付款链接已复制。请在导入 Cookie 的同一浏览器会话中打开。");
  } catch (err) {
    pushLog(`复制付款链接失败，请手动选中链接复制：${String(err)}`);
  }
}

/** 复制一段任意文本，用于把订单和付款链接发给客户。 */
export async function copyText(text: string, successMessage: string): Promise<void> {
  if (!text.trim()) return;
  try {
    await navigator.clipboard.writeText(text);
    pushLog(successMessage);
  } catch (err) {
    pushLog(`复制失败，请手动选中文本复制：${String(err)}`);
  }
}

/**
 * 从 Apple 官网抓最新型号。
 *
 * 只抓当前品类的那几页。全部品类加起来有二十页、几十兆 HTML，用户想看新出的
 * Mac 没有理由等着 iPhone、iPad、Watch 一起抓完。
 */
export async function refreshProducts(): Promise<void> {
  if (state.refreshing) return;
  update({ refreshing: true });
  const locale = state.settings.locale;
  const category = state.category;
  try {
    const count = await invoke<number>("refresh_products", { locale, category });
    // 说「抓到」而不是「更新」：这个数字是本轮成功抓下来的不同零件号数，
    // 不等于目录里真的多了或改了多少行。
    pushLog(`已从 Apple 官网抓到 ${count} 个型号。`);
  } catch (err) {
    // 抓取失败仍可继续用内嵌目录，但失败页面对应的旧型号可能已经停售。
    // 必须把这一层风险写出来；只说「仍可使用」会让旧快照显得像可信的当前目录。
    pushLog(
      `更新型号列表失败（失败页面继续使用内置旧目录，其中的型号可能已过期）：${String(err)}`,
    );
  } finally {
    // 无论成败都重载一次目录。**失败时也必须重载**：后端是一页一页安装的，
    // 一个品类有八页，其中几页成功、几页失败是常事，成功那几页的新数据此刻
    // 已经在后端生效了。不重载的话界面还停在旧目录上，等到下一次因为别的
    // 原因重载时，这批变更才悄悄冒出来 —— 那时候已经没有任何提示说明它们
    // 是哪来的了。
    await loadCatalog(locale);
    update({ refreshing: false });
  }
}

export async function testNotify(): Promise<void> {
  try {
    await invoke("test_notify");
    pushLog("已发出测试提醒。");
  } catch (err) {
    pushLog(`测试提醒失败：${String(err)}`);
  }
}

// ---- 应用更新。只提示，不静默安装。

export async function checkForUpdate(opts?: { quiet?: boolean }): Promise<void> {
  try {
    const info = await invoke<UpdateInfo | null>("check_for_update");
    update({ update: info });
    if (!opts?.quiet) {
      pushLog(info ? `发现新版本 ${info.version}。` : "已经是最新版本。");
    }
  } catch (err) {
    // 检查更新失败不是故障，只是这次没查到。静默模式下连日志都不写，
    // 免得每次断网启动都刷一条无用信息。
    if (!opts?.quiet) pushLog(`检查更新失败：${String(err)}`);
  }
}

/**
 * 下载并安装更新。
 *
 * 刻意由用户点击触发，绝不静默进行：这是个会在抢购当口挂着的程序，
 * 自作主张地下载、替换、重启，正好会赶上最不该被打断的时刻。
 */
export async function installUpdate(): Promise<void> {
  if (state.installing) return;
  update({ installing: true });
  try {
    await invoke("install_update");
    pushLog("更新已安装，重启应用后生效。");
  } catch (err) {
    pushLog(`安装更新失败：${String(err)}`);
  } finally {
    update({ installing: false });
  }
}

export function dismissUpdate(): void {
  update({ update: null });
}
