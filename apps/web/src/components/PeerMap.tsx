import { PeerInfo } from '../api'

/** How a peer edge should be drawn on the map. */
export type EdgeKind = 'lan' | 'relay' | 'internet' | 'idle'

export function edgeKind(peer: PeerInfo): EdgeKind {
  if (!peer.conn) return 'idle'
  if (peer.conn.transport === 'relay') return 'relay'
  return peer.conn.lan ? 'lan' : 'internet'
}

export function agoLabel(ms: number): string {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000))
  if (s < 60) return 'just now'
  if (s < 3600) return `${Math.round(s / 60)}m ago`
  if (s < 48 * 3600) return `${Math.round(s / 3600)}h ago`
  return `${Math.round(s / 86400)}d ago`
}

export interface MapPeer {
  key: string
  name: string
  sub: string
  peer: PeerInfo
}

/**
 * The sync constellation: this device on the left, every known peer on the
 * right, edges showing the transport in use right now — direct LAN, direct
 * over the internet, or via a public relay (drawn as a waypoint). Arrowheads
 * point the way the connection is opened (who dials whom). Idle peers hang
 * on a dotted line with their last-contact time.
 */
export function PeerMap({ self, selfSub, peers }: { self: string; selfSub: string; peers: MapPeer[] }) {
  const W = 560
  const NODE_W = 168
  const NODE_H = 46
  const ROW = 78
  const H = Math.max(170, peers.length * ROW + 64)
  const selfX = 10
  const peerX = W - NODE_W - 10
  const selfY = H / 2
  const anyRelay = peers.some((p) => edgeKind(p.peer) === 'relay')
  const relay = { x: W / 2 - 34, y: 16, w: 68, h: 30 }
  const relayCx = relay.x + relay.w / 2
  const relayCy = relay.y + relay.h

  const peerY = (i: number) => 40 + ROW / 2 + i * ROW + (anyRelay ? 18 : 0)

  return (
    <svg
      className="peer-map"
      viewBox={`0 0 ${W} ${H}`}
      role="img"
      aria-label="map of sync peers and their transports"
    >
      <defs>
        <marker id="pm-arrow" viewBox="0 0 8 8" refX="7" refY="4" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
          <path d="M0,0.5 L8,4 L0,7.5" fill="none" stroke="context-stroke" strokeWidth="1.4" />
        </marker>
      </defs>

      {peers.map((p, i) => {
        const kind = edgeKind(p.peer)
        const y = peerY(i)
        const x1 = selfX + NODE_W
        const y1 = selfY
        const x2 = peerX
        const y2 = y
        // Arrow points the way the connection is opened; no arrow when we
        // don't know (contact predates origin tracking).
        const markerProps =
          p.peer.discovery === 'ticket' ? { markerEnd: 'url(#pm-arrow)' } :
          p.peer.discovery === 'inbound' ? { markerStart: 'url(#pm-arrow)' } : {}
        const label =
          kind === 'lan' ? 'LAN · p2p' :
          kind === 'internet' ? 'internet · p2p' :
          kind === 'relay' ? 'public relay · proxied' :
          `last sync ${agoLabel(p.peer.last_ok_ms)}`
        if (kind === 'relay') {
          const d = `M ${x1} ${y1} Q ${relayCx} ${relayCy + 8} ${x2} ${y2}`
          return (
            <g key={p.key} className={`pm-edge ${kind}`}>
              <path d={d} fill="none" {...markerProps} />
              <text x={(x1 + x2) / 2} y={(Math.min(y1, y2) + relayCy) / 2 + 2} textAnchor="middle" className="pm-label">
                {label}
              </text>
            </g>
          )
        }
        const mx = (x1 + x2) / 2
        const my = (y1 + y2) / 2
        return (
          <g key={p.key} className={`pm-edge ${kind}`}>
            <line x1={x1} y1={y1} x2={x2} y2={y2} {...markerProps} />
            <text x={mx} y={my - 7} textAnchor="middle" className="pm-label" transform={`rotate(${(Math.atan2(y2 - y1, x2 - x1) * 180) / Math.PI} ${mx} ${my})`}>
              {label}
            </text>
          </g>
        )
      })}

      {anyRelay && (
        <g className="pm-node pm-relay">
          <rect x={relay.x} y={relay.y} width={relay.w} height={relay.h} rx={15} />
          <text x={relayCx} y={relay.y + relay.h / 2 + 4} textAnchor="middle">relay</text>
        </g>
      )}

      <g className="pm-node pm-self">
        <rect x={selfX} y={selfY - NODE_H / 2} width={NODE_W} height={NODE_H} rx={6} />
        <text x={selfX + 14} y={selfY - 3} className="pm-name">{self}</text>
        <text x={selfX + 14} y={selfY + 13} className="pm-sub">{selfSub}</text>
      </g>

      {peers.map((p, i) => {
        const y = peerY(i)
        const active = edgeKind(p.peer) !== 'idle'
        return (
          <g key={p.key} className={`pm-node pm-peer${active ? ' online' : ''}`}>
            <rect x={peerX} y={y - NODE_H / 2} width={NODE_W} height={NODE_H} rx={6} />
            <circle cx={peerX + NODE_W - 13} cy={y - NODE_H / 2 + 13} r={3.5} className="pm-dot" />
            <text x={peerX + 14} y={y - 3} className="pm-name">{p.name}</text>
            <text x={peerX + 14} y={y + 13} className="pm-sub">{p.sub}</text>
          </g>
        )
      })}
    </svg>
  )
}
