import { useCallback, useEffect, useId, useMemo, useRef, useState, type PointerEvent } from 'react'
import { api } from '../api'
import { CanvasController, DEFAULT_SETTINGS, type Settings, type UiTool } from './controller'
import { CanvasData } from './sync'
import { onAppearanceChange, readAppearance, setAppearance } from '../appearance'
import { renderSample, type Layers, type Tool, type View } from './ink'
import { isMonthKey, loadJSON, monthKey, monthLabel, monthsWithContent, prevMonth, saveJSON } from './store'
import './canvas.css'

const K_TOOLS = 'memorious.canvas.tools'
const K_LAYERS = 'memorious.canvas.layers'
const K_MONTHS = 'memorious.canvas.months'
const K_VIEW = 'memorious.canvas.view'
const K_TOUCH = 'memorious.canvas.touch'
const K_MINI = 'memorious.canvas.minimap'
const K_DIM = 'memorious.canvas.dim' // 'off' = earlier periods at full brightness; anything else = dimmed

interface MonthPrefs {
  visible: string[]
  extra: string[]
}

const isDraw = (t: UiTool): t is Tool => t === 'pencil' || t === 'pen' || t === 'eraser'
const isDark = () => {
  const a = readAppearance()
  return a === 'dark' || (a === 'system' && window.matchMedia('(prefers-color-scheme: dark)').matches)
}

/** Line icons (stroke = currentColor). */
const Icon = ({ d, size = 20, dash }: { d: string; size?: number; dash?: boolean }) => (
  <svg width={size} height={size} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round" strokeDasharray={dash ? '3 3' : undefined} aria-hidden>
    <path d={d} />
  </svg>
)
const ICONS = {
  swap: 'M7 4L3 8l4 4M3 8h14M17 20l4-4-4-4M21 16H7',
  undo: 'M9 14L4 9l5-5M4 9h10a6 6 0 010 12h-3',
  redo: 'M15 14l5-5-5-5M20 9H10a6 6 0 000 12h3',
  move: 'M12 3v18M3 12h18M9 6l3-3 3 3M9 18l3 3 3-3M6 9l-3 3 3 3M18 9l3 3-3 3',
  trash: 'M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3',
  copy: 'M9 9h11v11H9zM5 15V4h11',
  pencil: 'M4 20l1-4L16 5l3 3L8 19zM14 7l3 3',
  pen: 'M16 3.5l4.5 4.5L8 20.5 3 21.5l1-5z', // a pen: the pencil's shape without the eraser band — drawn solid, with the nib's slit cut out (PenIcon)
  eraser: 'M7 21h12M5 15l9-9 5 5-7 7H8z',
  lasso: 'M6 9c0-3 6-5 10-3s3 7-1 9-8 1-9-2M10 17c0 2 1 3 3 4',
  rect: 'M5 5h14v14H5z',
  timeline: 'M8 8c0-3 6-5 10-3s3 7-1 9-8 1-9-2M12 16c0 2 1 3 3 4M2 5h4M2 9h3M2 13h2', // a lasso with stacked lines: select through the periods underneath
  image: 'M4 5h16v14H4zM4 16l5-5 4 4 3-3 4 4M9 9v.01',
  options: 'M4 7h10M18 7h2M4 17h2M10 17h10M14 4v6M8 14v6',
  sun: 'M12 8a4 4 0 100 8 4 4 0 000-8zM12 2v2M12 20v2M2 12h2M20 12h2M5 5l1.5 1.5M17.5 17.5L19 19M5 19l1.5-1.5M17.5 6.5L19 5',
  moon: 'M20 14A8 8 0 1110 4a6.5 6.5 0 0010 10z',
}
/** The pen is the one solid icon: a filled body with the nib's slit knocked out (a mask, so it works on any button colour), drawn a
 *  little smaller than the outline icons so its weight matches the pencil's. */
function PenIcon({ size = 20 }: { size?: number }) {
  const id = useId()
  return (
    <svg width={size} height={size} viewBox="0 0 24 24" aria-hidden>
      <mask id={id}>
        <rect width="24" height="24" fill="white" />
        <path d="M5.2 19.2L10.2 14.2" stroke="black" strokeWidth="1.7" strokeLinecap="round" />
      </mask>
      <g transform="translate(12.1 12.1) scale(0.86) translate(-12 -12)">
        <path d={ICONS.pen} fill="currentColor" stroke="currentColor" strokeWidth="1.2" strokeLinejoin="round" mask={`url(#${id})`} />
      </g>
    </svg>
  )
}
const TOOLS: UiTool[] = ['pencil', 'pen', 'eraser', 'lasso', 'rect', 'timeline']
/** One key per tool (shown in the tool list and the tooltips). Undo/redo are the usual Cmd/Ctrl+Z / Shift+Z. With a selection: C duplicates, Q deletes. */
const KEYS: Record<UiTool | 'image', string> = { pencil: 'D', pen: 'W', eraser: 'E', lasso: 'S', rect: 'R', timeline: 'T', image: 'G' } // all on the left of the keyboard: one hand on the keys, the other on the pencil
const SWAP_KEY = 'X' // back to the previous tool
const MOD = /Mac|iPhone|iPad/.test(navigator.platform) ? '⌘' : 'Ctrl+'
const toolIcon = (t: UiTool) => (t === 'pen' ? <PenIcon /> : <Icon d={ICONS[t]} dash={t === 'rect' || t === 'lasso' || t === 'timeline'} />)

const MARK_NUDGE: Record<string, [number, number]> = {
  ArrowLeft: [-10, 0],
  ArrowRight: [10, 0],
  ArrowUp: [0, -10],
  ArrowDown: [0, 10],
}

function MoveMarker({ onMove }: { onMove(dx: number, dy: number): void }) {
  const drag = useRef<{ id: number; x: number; y: number } | null>(null)
  const move = (e: PointerEvent<HTMLButtonElement>) => {
    const d = drag.current
    if (!d || d.id !== e.pointerId) return
    onMove(e.clientX - d.x, e.clientY - d.y)
    d.x = e.clientX
    d.y = e.clientY
  }
  return (
    <button
      type="button"
      className="canvas-fab canvas-move-marker"
      aria-label="Move marker"
      title="Drag to move marker"
      onPointerDown={(e) => {
        if (drag.current || e.button !== 0) return
        e.currentTarget.setPointerCapture(e.pointerId)
        drag.current = { id: e.pointerId, x: e.clientX, y: e.clientY }
      }}
      onPointerMove={move}
      onPointerUp={move}
      onLostPointerCapture={(e) => {
        if (drag.current?.id === e.pointerId) drag.current = null
      }}
      onContextMenu={(e) => e.preventDefault()}
      onKeyDown={(e) => {
        const delta = MARK_NUDGE[e.key]
        if (!delta) return
        e.preventDefault()
        onMove(...delta)
      }}
    >
      <Icon d={ICONS.move} />
    </button>
  )
}

interface SettingsProps {
  tool: UiTool
  settings: Settings
  onSetting(t: Tool, s: { size?: number; op?: number }): void
  zoom: number
}
/** Size + opacity of the current drawing tool (selection tools have none). */
function Sliders({ tool, settings, onSetting, zoom }: SettingsProps) {
  const sample = useRef<HTMLCanvasElement>(null)
  const drawing = isDraw(tool)
  const size = drawing ? settings[tool].size : 0
  const op = drawing ? settings[tool].op : 0
  useEffect(() => {
    if (sample.current && drawing) renderSample(sample.current, tool as Tool, size, op, zoom)
  }, [tool, drawing, size, op, zoom])
  if (!isDraw(tool)) return null
  const s = settings[tool]
  return (
    <>
      <canvas ref={sample} className="canvas-sample" />
      <label className="canvas-slider">
        <span>size</span>
        <input type="range" min={1} max={24} step={0.5} value={s.size} onChange={(e) => onSetting(tool, { size: +e.target.value })} />
        <b>{s.size}</b>
      </label>
      <label className="canvas-slider">
        <span>opacity</span>
        <input type="range" min={0.05} max={1} step={0.05} value={s.op} onChange={(e) => onSetting(tool, { op: +e.target.value })} />
        <b>{s.op.toFixed(2)}</b>
      </label>
    </>
  )
}

interface ToolListProps {
  tool: UiTool
  onTool(t: UiTool): void
  onImage(): void
}
function ToolList({ tool, onTool, onImage }: ToolListProps) {
  return (
    <div className="canvas-tools">
      {TOOLS.map((t) => (
        <button key={t} className={'canvas-btn wide' + (tool === t ? ' on' : '')} onClick={() => onTool(t)}>
          {toolIcon(t)}
          {t}
          <kbd className="canvas-key">{KEYS[t]}</kbd>
        </button>
      ))}
      <button className="canvas-btn wide" onClick={onImage}>
        <Icon d={ICONS.image} />
        image
        <kbd className="canvas-key">{KEYS.image}</kbd>
      </button>
    </div>
  )
}

export function CanvasView() {
  const host = useRef<HTMLDivElement>(null)
  const file = useRef<HTMLInputElement>(null)
  const ctl = useRef<CanvasController | null>(null)
  const now = useMemo(() => monthKey(), [])
  const prev = useMemo(() => prevMonth(now), [now])

  const [tool, setToolState] = useState<UiTool>('pencil')
  const [prevTool, setPrevTool] = useState<UiTool | null>(null)
  const [dark, setDark] = useState(isDark)
  const [touch, setTouch] = useState<boolean>(() => {
    const v = localStorage.getItem(K_TOUCH)
    return v ? v === 'on' : window.matchMedia('(pointer: coarse)').matches // phones draw with a finger by default
  })
  const [hasMark, setHasMark] = useState(false)
  const [zoom, setZoom] = useState(1)
  const [menu, setMenu] = useState<{ x: number; y: number } | null>(null)
  const [mini, setMini] = useState(() => localStorage.getItem(K_MINI) === 'on')
  const [dim, setDim] = useState(() => localStorage.getItem(K_DIM) !== 'off')
  const [hideCur, setHideCur] = useState(false) // this period hidden, to see the earlier ones underneath (not remembered)
  const [panel, setPanel] = useState<'tools' | 'options' | null>(null)
  const [corner, setCorner] = useState<{ x: number; y: number } | null>(null)
  const [settings, setSettings] = useState<Settings>(() => {
    const saved = loadJSON<Partial<Settings>>(K_TOOLS, {})
    return Object.fromEntries((Object.keys(DEFAULT_SETTINGS) as Tool[]).map((t) => [t, { ...DEFAULT_SETTINGS[t], ...saved[t] }])) as Settings
  })
  const [layers, setLayers] = useState<Layers>(() => loadJSON(K_LAYERS, { pencil: true, pen: true, image: true }))
  const [prefs, setPrefs] = useState<MonthPrefs>(() => loadJSON(K_MONTHS, { visible: [prev], extra: [] }))
  const [floating, setFloating] = useState(false)
  const [tick, setTick] = useState(0)
  const [remote, setRemote] = useState<string[]>([])

  // controller lifetime = view lifetime
  useEffect(() => {
    let c: CanvasController | null = null
    const data = new CanvasData(api.ink, (m) => c?.remoteChanged(m))
    const refreshMonths = () => void data.remoteMonths().then(setRemote)
    refreshMonths()
    const monthsTimer = window.setInterval(refreshMonths, 30000)
    c = new CanvasController(
      host.current!,
      {
        onChange: () => {
          setTick((t) => t + 1)
          if (c) saveJSON(K_VIEW, c.view)
        },
        onFloating: setFloating,
        onTool: (t, prev) => {
          setToolState(t)
          setPrevTool(prev)
        },
        onZoom: setZoom,
        onSelection: setCorner,
        onContext: (x, y) => setMenu({ x, y }),
        onTouchDraws: (on) => setTouch(on),
        onMark: setHasMark,
        onHideCurrent: setHideCur,
      },
      data,
      { settings, view: loadJSON<View>(K_VIEW, { x: 0, y: 0, z: 1 }), touchDraws: touch },
    )
    ctl.current = c
    c.layers = layers
    c.setVisibleMonths(prefs.visible)
    c.setMinimap(mini)
    c.setDim(dim)
    c.setTool('pencil')
    c.rebuild()
    return () => {
      window.clearInterval(monthsTimer)
      c?.dispose()
      data.dispose()
      ctl.current = null
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const months = useMemo(() => {
    const all = new Set([now, prev, ...monthsWithContent(), ...remote, ...prefs.extra])
    return [...all].sort().reverse()
    // tick: a stroke may have made the current month non-empty
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [now, prev, prefs.extra, remote, tick])

  useEffect(() => {
    const f = () => setDark(isDark())
    const m = window.matchMedia('(prefers-color-scheme: dark)')
    m.addEventListener('change', f)
    const off = onAppearanceChange(f)
    return () => {
      m.removeEventListener('change', f)
      off()
    }
  }, [])

  const pickTool = useCallback((t: UiTool) => ctl.current?.setTool(t), []) // the controller reports back through onTool
  const onSetting = useCallback((t: Tool, s: { size?: number; op?: number }) => {
    ctl.current?.setSetting(t, s)
    setSettings((cur) => {
      const next = { ...cur, [t]: { ...cur[t], ...s } }
      saveJSON(K_TOOLS, next)
      return next
    })
  }, [])
  const toggleLayer = (l: keyof Layers) => {
    const next = { ...layers, [l]: !layers[l] }
    setLayers(next)
    saveJSON(K_LAYERS, next)
    ctl.current?.setLayers(next)
  }
  const toggleMonth = (m: string) => {
    const visible = prefs.visible.includes(m) ? prefs.visible.filter((x) => x !== m) : [...prefs.visible, m]
    const next = { ...prefs, visible }
    setPrefs(next)
    saveJSON(K_MONTHS, next)
    ctl.current?.setVisibleMonths(visible)
  }
  const addMonth = (m: string) => {
    if (!isMonthKey(m) || m === now) return
    const next = { extra: [...new Set([...prefs.extra, m])], visible: [...new Set([...prefs.visible, m])] }
    setPrefs(next)
    saveJSON(K_MONTHS, next)
    ctl.current?.setVisibleMonths(next.visible)
  }

  // close the popovers on any outside press / Escape
  useEffect(() => {
    if (!panel && !menu) return
    const close = (e: Event) => {
      if (!(e.target as HTMLElement).closest?.('.canvas-menu, .canvas-pop, .canvas-fabs')) {
        setMenu(null)
        setPanel(null)
      }
    }
    const esc = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        setMenu(null)
        setPanel(null)
      }
    }
    window.addEventListener('pointerdown', close, true)
    window.addEventListener('keydown', esc)
    return () => {
      window.removeEventListener('pointerdown', close, true)
      window.removeEventListener('keydown', esc)
    }
  }, [panel, menu])

  const chooseTool = (t: UiTool) => {
    pickTool(t)
    setPanel(null)
    setMenu(null)
  }
  const chooseImage = () => {
    setPanel(null)
    setMenu(null)
    file.current?.click()
  }
  // tool shortcuts (single letters; ignored while typing or with a modifier held)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey || /^(INPUT|TEXTAREA|SELECT)$/.test((e.target as HTMLElement)?.tagName)) return
      const k = e.key.toUpperCase()
      if (k === SWAP_KEY) {
        e.preventDefault()
        ctl.current?.swapTool()
        return
      }
      if (k === KEYS.image) {
        e.preventDefault()
        file.current?.click()
        return
      }
      const t = TOOLS.find((x) => KEYS[x] === k)
      if (t) {
        e.preventDefault()
        pickTool(t)
        setPanel(null)
        setMenu(null)
      }
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [pickTool])
  const undoRedo = (
    <div className="canvas-row">
      <button className="canvas-btn icon" title={`undo (${MOD}Z)`} aria-label="undo" disabled={!ctl.current?.canUndo} onClick={() => ctl.current?.undo()}>
        <Icon d={ICONS.undo} />
      </button>
      <button className="canvas-btn icon" title={`redo (${MOD}⇧Z)`} aria-label="redo" disabled={!ctl.current?.canRedo} onClick={() => ctl.current?.redo()}>
        <Icon d={ICONS.redo} />
      </button>
    </div>
  )
  const toggleHideCurrent = () => {
    const next = !hideCur
    setHideCur(next)
    ctl.current?.setHideCurrent(next)
  }
  const toggleDim = () => {
    const next = !dim
    setDim(next)
    localStorage.setItem(K_DIM, next ? 'on' : 'off')
    ctl.current?.setDim(next)
  }
  const toggleMini = () => {
    const next = !mini
    setMini(next)
    localStorage.setItem(K_MINI, next ? 'on' : 'off')
    ctl.current?.setMinimap(next)
  }
  // the layer toggles, in the options panel and the right-click menu
  const layerChecks = (
    <div className="canvas-checks">
      {(['pencil', 'pen', 'image'] as (keyof Layers)[]).map((l) => (
        <label key={l} className="canvas-check">
          <input type="checkbox" checked={layers[l]} onChange={() => toggleLayer(l)} />
          {l === 'image' ? 'images' : l}
        </label>
      ))}
    </div>
  )
  const toggleTouch = () => {
    const next = !touch
    setTouch(next)
    localStorage.setItem(K_TOUCH, next ? 'on' : 'off')
    if (ctl.current) ctl.current.touchDraws = next
  }

  return (
    <div className={'canvas-view' + (mini ? ' mini-on' : '')}>
      <div className="canvas-host" ref={host} />

      {/* floating buttons: quick tool picker, options, undo, redo */}
      <div className="canvas-fabs">
        <button
          className={'canvas-fab' + (panel === 'tools' ? ' on' : '')}
          title={`${tool} (${KEYS[tool]})`}
          aria-label="tools"
          onClick={() => setPanel(panel === 'tools' ? null : 'tools')}
        >
          {toolIcon(tool)}
        </button>
        <button
          className="canvas-fab"
          title={prevTool ? `previous tool: ${prevTool} (${SWAP_KEY})` : 'previous tool'}
          aria-label="previous tool"
          disabled={!prevTool}
          onClick={() => ctl.current?.swapTool()}
        >
          {prevTool ? toolIcon(prevTool) : <Icon d={ICONS.swap} />}
        </button>
        <button
          className={'canvas-fab' + (panel === 'options' ? ' on' : '')}
          title="options"
          aria-label="options"
          onClick={() => setPanel(panel === 'options' ? null : 'options')}
        >
          <Icon d={ICONS.options} />
        </button>
        <button className="canvas-fab" title={`undo (${MOD}Z)`} aria-label="undo" disabled={!ctl.current?.canUndo} onClick={() => ctl.current?.undo()}>
          <Icon d={ICONS.undo} />
        </button>
        <button className="canvas-fab" title={`redo (${MOD}⇧Z)`} aria-label="redo" disabled={!ctl.current?.canRedo} onClick={() => ctl.current?.redo()}>
          <Icon d={ICONS.redo} />
        </button>
      </div>

      {panel === 'tools' && (
        <div className="canvas-pop tools">
          <ToolList tool={tool} onTool={chooseTool} onImage={chooseImage} />
        </div>
      )}

      {panel === 'options' && (
        <div className="canvas-pop options">
          <Sliders tool={tool} settings={settings} onSetting={onSetting} zoom={ctl.current?.view.z ?? 1} />
          <div className="canvas-row">
            <button
              className="canvas-btn icon"
              title={dark ? 'light' : 'dark'}
              aria-label={dark ? 'light' : 'dark'}
              onClick={() => setAppearance(dark ? 'light' : 'dark')}
            >
              <Icon d={dark ? ICONS.sun : ICONS.moon} />
            </button>
            <button className={'canvas-btn' + (touch ? ' on' : '')} onClick={toggleTouch}>
              finger
            </button>
            <button className={'canvas-btn' + (mini ? ' on' : '')} onClick={toggleMini}>
              minimap
            </button>
            <button className={'canvas-btn' + (dim ? ' on' : '')} onClick={toggleDim} title="earlier periods: dimmed, or full brightness">
              dim earlier
            </button>

          </div>
          <hr />
          {layerChecks}
          <hr />
          <div className="canvas-checks">
            {months.map((m) => (
              <label key={m} className="canvas-check">
                <input type="checkbox" checked={m === now ? !hideCur : prefs.visible.includes(m)} onChange={() => (m === now ? toggleHideCurrent() : toggleMonth(m))} />
                {monthLabel(m)}
              </label>
            ))}
            {/* the real month input sits invisibly over this label, so tapping "other…" opens the native picker everywhere */}
            <label className="canvas-btn canvas-other">
              other…
              <input
                type="month"
                aria-label="other month"
                max={now}
                onChange={(e) => {
                  addMonth(e.target.value)
                  e.target.value = ''
                }}
              />
            </label>
          </div>
        </div>
      )}

      {/* zoom: out / percent (tap = back to 100%) / in, under the minimap; without the minimap just a reset button */}
      {mini ? (
        <div className="canvas-zoom">
          <button className="canvas-btn" title="zoom out" aria-label="zoom out" onClick={() => ctl.current?.zoomBy(1 / 1.25)}>
            <Icon d="M5 12h14" size={16} />
          </button>
          <button className="canvas-btn canvas-zoom-reset" title="reset zoom to 100%" aria-label="reset zoom" onClick={() => ctl.current?.resetZoom()}>
            {Math.round(zoom * 100)}%
          </button>
          <button className="canvas-btn" title="zoom in" aria-label="zoom in" onClick={() => ctl.current?.zoomBy(1.25)}>
            <Icon d="M12 5v14M5 12h14" size={16} />
          </button>
        </div>
      ) : (
        <button className="canvas-btn canvas-zoom-solo" aria-label="reset zoom" onClick={() => ctl.current?.resetZoom()}>
          Reset zoom
        </button>
      )}

      {touch && hasMark && !floating && <MoveMarker onMove={(dx, dy) => ctl.current?.moveMarkBy(dx, dy)} />}

      {floating && (
        <div className="canvas-placebar">
          <button className="canvas-btn on" onClick={() => ctl.current?.placeImage()}>
            place
          </button>
          <button className="canvas-btn" onClick={() => ctl.current?.cancelImage()}>
            cancel
          </button>
        </div>
      )}

      {corner && (
        <div className="canvas-selbar" style={{ left: Math.max(corner.x, 100), top: Math.max(corner.y, 54) }}>
          <button className="canvas-btn icon" title={`duplicate (C, ${MOD}D)`} aria-label="duplicate" onClick={() => ctl.current?.duplicateSelection()}>
            <Icon d={ICONS.copy} />
          </button>
          <button className="canvas-btn icon" title="delete (Q, Del)" aria-label="delete" onClick={() => ctl.current?.deleteSelection()}>
            <Icon d={ICONS.trash} />
          </button>
        </div>
      )}

      {menu && (
        <div className="canvas-menu" style={{ left: Math.min(menu.x, window.innerWidth - 240), top: Math.max(8, Math.min(menu.y, window.innerHeight - 580)) }}>
          <ToolList tool={tool} onTool={chooseTool} onImage={chooseImage} />
          <Sliders tool={tool} settings={settings} onSetting={onSetting} zoom={ctl.current?.view.z ?? 1} />
          {undoRedo}
          <hr />
          {layerChecks}
        </div>
      )}

      <input
        ref={file}
        type="file"
        accept="image/*"
        hidden
        onChange={(e) => {
          const f = e.target.files?.[0]
          if (f) void ctl.current?.addImage(f)
          e.target.value = ''
        }}
      />
    </div>
  )
}
