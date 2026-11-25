export type DeviceView = {
  id: string;
  name: string;
  os: string;
  version: string;
  online: boolean;
  lastSeen: number;
  controllable: boolean;
  /** UI 展示用，后端暂未返回时由 mock 数据补齐 */
  ip?: string;
  resolution?: string;
  latency?: number;
  cpu?: number;
  mem?: number;
  gpu?: string;
  location?: string;
};

export type AccountView = {
  serverUrl: string;
  username: string;
  deviceId: string;
  loggedIn: boolean;
  connected: boolean;
  error: string | null;
  /** 连接码临时会话：不是账号登录，隐藏账号相关入口。 */
  quick?: boolean;
  /** 本机模式：不连服务器，只用局域网直连。 */
  local?: boolean;
};

/** 局域网直连状态（对应客户端的 get_direct_state 等命令）。 */
export type DirectState = {
  /** 用户是否打开了直连（写进配置）。 */
  enabled: boolean;
  /** 实际是否在监听（可能因端口占用为 false）。 */
  listening: boolean;
  /** 默认 47900。 */
  port: number;
  /** 本机局域网 IPv4，界面直接显示给用户。 */
  addresses: string[];
  /** 本机识别码（9 位数字）；不登录也有，同网段输入它就能找到本机。 */
  localId: string;
  /** 6 位数字；未开启时 null。 */
  pairingCode: string | null;
  /** 配对码剩余秒数；未开启时 0。 */
  pairingExpiresIn: number;
  /** 当前直连进来的会话数。 */
  activeSessions: number;
  /** 已授权可以直连本机的设备数（配对过一次的长期凭据条数）。 */
  trustedPeers: number;
};

/** Windows 防火墙里给直连放行的规则状态。 */
export type DirectFirewallState = {
  /** 同名规则是否存在。 */
  rulePresent: boolean;
  /** 规则是否覆盖当前程序与监听端口。 */
  ruleMatches: boolean;
  /** 规则名（ScreenWall Direct）。 */
  ruleName: string;
  /** 需要放行的监听端口。 */
  port: number;
};

export type PageKey =
  | "remote"
  | "wall"
  | "group"
  | "files"
  | "devices"
  | "audit"
  | "settings"
  | "server"
  | "control";

export type ToastTone = "info" | "success" | "error";

export type ToastMessage = {
  id: number;
  text: string;
  tone: ToastTone;
};

export type Notify = (text: string, tone?: ToastTone) => void;

export type PageProps = {
  account: AccountView;
  devices: DeviceView[];
  notify: Notify;
};

export type WallPageProps = PageProps & {
  selectedIds: string[];
  onToggleSelect: (id: string) => void;
  onClearSelection: () => void;
  onOpenDevice: (id: string) => void;
  onGroup: () => void;
  onFiles: (deviceId?: string) => void;
  onDevices: () => void;
};

export type ControlPageProps = PageProps & {
  deviceId: string;
  onBack: () => void;
  onFiles: () => void;
  onOpenDevice: (id: string) => void;
};

export type GroupPageProps = PageProps & {
  selectedIds: string[];
  setSelectedIds: (ids: string[]) => void;
  onOpenDevice: (id: string) => void;
};

export type FilesPageProps = PageProps & {
  initialDeviceId?: string | null;
  onOpenWall: () => void;
};

export type DevicesPageProps = PageProps & {
  onOpenDevice: (id: string) => void;
};

export type RemotePageProps = PageProps & {
  /** 连上之后直接进控制页，不用用户再去设备列表里找一遍。 */
  onOpenDevice: (id: string) => void;
};

export type AuditPageProps = PageProps;

export type SettingsPageProps = PageProps & {
  onLogout: () => void;
  busy: boolean;
};
