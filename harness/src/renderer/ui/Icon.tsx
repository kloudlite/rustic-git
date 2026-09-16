import { Dynamic } from "solid-js/web";
import {
  Archive, ArrowDownLeft, ArrowUpRight, Bell, Bot, Box, Camera, Check, ChevronDown, ChevronLeft, ChevronRight, ChevronsUpDown, Circle, Clock, Columns2, Container, Copy, Ellipsis, ExternalLink, FolderGit2, Globe, LoaderCircle,
  File, FileDiff, Folder, FolderOpen, GitBranch, GitCommitHorizontal, History, Lock, Monitor,
  Maximize2, MessageSquare, Minimize2, Moon, PanelLeft, PanelRight, Plus, Search, Server, Settings, Sparkles, SquareDashedMousePointer, Sun, Target, Terminal, User, Users, X,
} from "lucide-solid";
import { cx } from "./cx";

/**
 * Lucide, behind one name per role. This is the same set Zed ships — its
 * assets/icons are Lucide under the ISC licence, redrawn onto a 16px grid — so
 * using the library directly keeps the app's glyphs and Zed's identical without
 * vendoring anything.
 *
 * Call sites name the role ("folder", "running"), never the glyph, so the whole
 * app changes together if a role ever needs a different picture.
 *
 * Stroke is 1.75, not Lucide's default 2: Zed draws at 1.2 on a 16px grid, which
 * is 1.05px on screen at our 14px icons, and 1.75 on Lucide's 24px grid matches
 * it. The default reads as heavy beside IBM Plex at the same size.
 */
const ICONS = {
  sparkle: Sparkles,
  folder: Folder,
  folderOpen: FolderOpen,
  file: File,
  diff: FileDiff,
  branch: GitBranch,
  git: GitCommitHorizontal,
  chevronDown: ChevronDown,
  chevronRight: ChevronRight,
  chevronLeft: ChevronLeft,
  search: Search,
  split: Columns2,
  panelLeft: PanelLeft,
  panelRight: PanelRight,
  plus: Plus,
  x: X,
  check: Check,
  dot: Circle,
  dotFilled: Circle,
  clock: Clock,
  terminal: Terminal,
  maximise: Maximize2,
  minimise: Minimize2,
  server: Server,
  more: Ellipsis,
  archive: Archive,
  thread: MessageSquare,
  spinner: LoaderCircle,
  container: Container,
  repo: FolderGit2,
  user: User,
  pick: SquareDashedMousePointer,
  machine: Target,
  workspace: Box,
  ephemeral: Bot,
  settings: Settings,
  globe: Globe,
  "external-link": ExternalLink,
  chevronsUpDown: ChevronsUpDown,
  users: Users,
  copy: Copy,
  history: History,
  camera: Camera,
  lock: Lock,
  arrowUpRight: ArrowUpRight,
  arrowDownLeft: ArrowDownLeft,
  bell: Bell,
  sun: Sun,
  moon: Moon,
  monitor: Monitor,
} as const;

export type IconName = keyof typeof ICONS;

/** The few that read as a solid mark rather than an outline. */
const FILLED = new Set<IconName>(["dotFilled"]);

export function Icon(props: { name: IconName | string; size?: number; class?: string }) {
  const known = () => (props.name in ICONS ? (props.name as IconName) : "dot");
  return (
    <Dynamic
      component={ICONS[known()]}
      size={props.size ?? 16}
      stroke-width={FILLED.has(known()) ? 0 : 1.5}
      fill={FILLED.has(known()) ? "currentColor" : "none"}
      class={cx("shrink-0", props.class)}
      aria-hidden="true"
    />
  );
}
