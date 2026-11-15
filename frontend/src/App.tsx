import { useEffect, useState } from 'react'
import { Toaster } from 'sonner'
import { listen, type UnlistenFn } from '@tauri-apps/api/event'
import { Sidebar, type PageKey } from './components/Sidebar'
import { HotkeyConflictDialog } from './components/HotkeyConflictDialog'
import { HomePage } from './pages/HomePage'
import { HistoryPage } from './pages/HistoryPage'
import { DictionaryPage } from './pages/DictionaryPage'
import { AppsPage } from './pages/AppsPage'
import { ProfilePage } from './pages/ProfilePage'
import { SettingsPage } from './pages/SettingsPage'
// v0.1.16 機能 L Day 4 → 2026-07-23 タブ統合: 議事録 (記録/履歴サブタブ)
import { MeetingTabsPage } from './pages/MeetingTabsPage'
// feature/seminar-subtitle: セミナー字幕モード
import { SeminarSubtitlePage } from './pages/SeminarSubtitlePage'
// 2026-07-28: プラン別ゲート (プロ限定機能)
import { ProFeatureGate } from './components/ProFeatureGate'
import type { MeetingStateEvent } from './types/meeting'

function App() {
  const [page, setPage] = useState<PageKey>('home')
  // v0.1.16 機能 L Day 4: 議事録録音中フラグ (CEO 推奨方針 4: HomePage 録音ボタンの併用抑止情報源)
  // 現状は Sidebar の議事録録音中 hint 用 reserved (HomePage の disable は Day 5 検討)。
  const [meetingRecording, setMeetingRecording] = useState(false)

  useEffect(() => {
    let mounted = true
    const unlistens: UnlistenFn[] = []
    ;(async () => {
      try {
        unlistens.push(
          await listen<MeetingStateEvent>('meeting:state-changed', (e) => {
            if (!mounted) return
            setMeetingRecording(e.payload.state === 'recording')
          }),
        )
      } catch (e) {
        console.warn('[app] meeting state listen failed:', e)
      }
    })()
    return () => {
      mounted = false
      for (const u of unlistens) u()
    }
  }, [])

  return (
    <div className="flex h-screen w-screen overflow-hidden bg-neutral-900 text-white antialiased">
      <Sidebar current={page} onChange={setPage} />
      {page === 'home' && <HomePage />}
      {page === 'history' && <HistoryPage />}
      {page === 'dictionary' && <DictionaryPage />}
      {page === 'apps' && <AppsPage />}
      {page === 'profile' && <ProfilePage />}
      {page === 'settings' && <SettingsPage />}
      {/* v0.1.16 機能 L Day 4 → 2026-07-23 タブ統合: 議事録 (記録/履歴サブタブ)
          2026-07-28: プロプラン限定機能としてゲート (trial / 未ログインは従来どおり通す) */}
      {page === 'meeting' && (
        <ProFeatureGate
          featureName="議事録モード"
          description="長時間の会議を録音し、話し言葉を整理した議事録として書き出す機能です。"
        >
          <MeetingTabsPage />
        </ProFeatureGate>
      )}
      {page === 'subtitle' && (
        <ProFeatureGate
          featureName="セミナー字幕"
          description="話した内容をその場で翻訳し、2 段字幕として表示する機能です。"
        >
          <SeminarSubtitlePage />
        </ProFeatureGate>
      )}
      {/* v0.1.10 機能 D: 起動時ホットキー競合ダイアログ (event listen ベース) */}
      <HotkeyConflictDialog onJumpToSettings={() => setPage('settings')} />
      <Toaster theme="dark" position="bottom-right" />
      {/* v0.1.16 機能 L Day 4: 議事録録音中の控えめなインジケータ (推奨方針 4 派生・併用注意喚起) */}
      {meetingRecording && page !== 'meeting' && (
        <div className="fixed top-2 right-2 z-40 bg-red-900/80 text-red-100 text-xs rounded px-3 py-1.5 shadow-lg pointer-events-none">
          ● 議事録録音中
        </div>
      )}
    </div>
  )
}

export default App
