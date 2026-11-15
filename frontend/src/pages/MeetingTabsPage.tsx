// MeetingTabsPage.tsx — タブ統合 (2026-07-23 要望: サイドバーのタブが増えすぎ)
// 「議事録」「議事録履歴」の 2 サイドバータブを 1 つに統合し、ページ内サブタブで切替える。
// MeetingPage / MeetingHistoryPage 本体は無改変で再利用 (録音は Rust 側で走るため
// サブタブ切替による unmount は従来のサイドバー切替と同じ挙動)。

import { useState } from 'react'
import { FileText, Clock } from 'lucide-react'
import { MeetingPage } from './MeetingPage'
import { MeetingHistoryPage } from './MeetingHistoryPage'
import { cn } from '../lib/cn'

type MeetingSubTab = 'record' | 'history'

const TABS: { key: MeetingSubTab; label: string; Icon: typeof FileText }[] = [
  { key: 'record', label: '記録', Icon: FileText },
  { key: 'history', label: '履歴', Icon: Clock },
]

export function MeetingTabsPage() {
  const [tab, setTab] = useState<MeetingSubTab>('record')
  return (
    <div className="flex-1 flex flex-col overflow-hidden">
      <div className="flex items-center gap-1 px-6 pt-4 border-b border-neutral-800">
        {TABS.map(({ key, label, Icon }) => (
          <button
            key={key}
            type="button"
            onClick={() => setTab(key)}
            className={cn(
              'flex items-center gap-2 px-4 py-2 text-sm rounded-t-md transition-colors border-b-2',
              tab === key
                ? 'text-white border-[var(--color-brand-500)] bg-neutral-950'
                : 'text-neutral-400 border-transparent hover:text-white hover:bg-neutral-900',
            )}
          >
            <Icon size={14} />
            <span>{label}</span>
          </button>
        ))}
      </div>
      <div className="flex-1 overflow-hidden flex">
        {tab === 'record' ? <MeetingPage /> : <MeetingHistoryPage />}
      </div>
    </div>
  )
}
