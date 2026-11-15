import { Mic, History, BookOpen, Settings, LayoutGrid, UserCog, FileText, Languages } from 'lucide-react'
import { cn } from '../lib/cn'

export type PageKey =
  | 'home'
  | 'history'
  | 'dictionary'
  | 'apps'
  | 'profile'
  | 'settings'
  // v0.1.16 機能 L Day 4 → 2026-07-23 タブ統合: 議事録 (記録/履歴はページ内サブタブ)
  | 'meeting'
  // feature/seminar-subtitle: セミナー字幕モード (海外講習向け)
  | 'subtitle'

interface SidebarProps {
  current: PageKey
  onChange: (page: PageKey) => void
}

const ITEMS: { key: PageKey; label: string; Icon: typeof Mic }[] = [
  { key: 'home', label: 'ホーム', Icon: Mic },
  { key: 'history', label: '履歴', Icon: History },
  { key: 'dictionary', label: '辞書', Icon: BookOpen },
  // Phase 2 C Wave 1 Step 2: 独立タブ新設 (本実装は Step 3 AT-06)
  { key: 'apps', label: 'アプリ', Icon: LayoutGrid },
  // Phase 2 C Wave 2: プロファイル独立タブ新設 (PT-01〜PT-10)
  { key: 'profile', label: 'プロファイル', Icon: UserCog },
  { key: 'settings', label: '設定', Icon: Settings },
  // 2026-07-23 タブ統合 (要望): 議事録+議事録履歴 → 1 タブ (ページ内サブタブ)
  { key: 'meeting', label: '議事録', Icon: FileText },
  // feature/seminar-subtitle: セミナー字幕 (末尾追加・既存並び順は変えない)
  { key: 'subtitle', label: 'セミナー字幕', Icon: Languages },
]

export function Sidebar({ current, onChange }: SidebarProps) {
  return (
    <nav className="w-[220px] h-screen bg-neutral-950 border-r border-neutral-800 flex flex-col">
      <div className="px-5 py-6 border-b border-neutral-800">
        <h1 className="text-lg font-semibold text-white tracking-wide">
          RushVoice
        </h1>
        <p className="text-xs text-neutral-500 mt-1">v{__APP_VERSION__}</p>
      </div>
      <ul className="flex-1 py-3">
        {ITEMS.map(({ key, label, Icon }) => {
          const active = current === key
          return (
            <li key={key}>
              <button
                type="button"
                onClick={() => onChange(key)}
                className={cn(
                  'w-full flex items-center gap-3 px-5 py-2.5 text-sm transition-colors',
                  active
                    ? 'bg-[var(--color-brand-700)]/20 text-white border-l-2 border-[var(--color-brand-500)]'
                    : 'text-neutral-400 hover:text-white hover:bg-neutral-900 border-l-2 border-transparent'
                )}
              >
                <Icon size={16} />
                <span>{label}</span>
              </button>
            </li>
          )
        })}
      </ul>
      <div className="px-5 py-4 border-t border-neutral-800 text-xs text-neutral-600">
        待機中
      </div>
    </nav>
  )
}
