import type {ReactNode} from 'react';
import Link from '@docusaurus/Link';
import Translate, {translate} from '@docusaurus/Translate';
import styles from './styles.module.css';

type Box = {x: number; y: number; w: number; label: string | string[]; accent?: boolean};

function DiagramNode({x, y, w, label, accent}: Box): ReactNode {
  const lines = Array.isArray(label) ? label : [label];
  const firstLineY = y + 33 - (lines.length - 1) * 9;
  return (
    <g>
      <rect x={x} y={y} width={w} height={56} rx={10} className={accent ? styles.nodeAccent : styles.node} />
      <text x={x + w / 2} y={firstLineY} textAnchor="middle" className={styles.label}>
        {lines.map((line, i) => (
          <tspan key={line} x={x + w / 2} dy={i === 0 ? 0 : 18}>
            {line}
          </tspan>
        ))}
      </text>
    </g>
  );
}

function Arrow({d}: {d: string}): ReactNode {
  return <path d={d} className={styles.arrow} markerEnd="url(#sy-arrow)" />;
}

export default function Architecture(): ReactNode {
  const lanes = [
    {y: 24, label: translate({id: 'home.arch.lane.standard', message: 'Standard lane'}), provider: ['OpenAI · Qwen', 'Vertex · vLLM']},
    {y: 112, label: translate({id: 'home.arch.lane.vertex', message: 'Native Vertex lane'}), provider: 'Vertex AI', accent: true},
    {y: 200, label: translate({id: 'home.arch.lane.jev', message: 'Jev lane'}), provider: 'TypeSafe', accent: true},
    {y: 288, label: translate({id: 'home.arch.lane.uhp', message: '2D UHP harness lane'}), provider: ['pi · claude-code', 'codex · hermes'], accent: true},
  ];
  return (
    <section className="sy-section">
      <div className="container">
        <p className="sy-eyebrow">
          <Translate id="home.arch.eyebrow">Architecture</Translate>
        </p>
        <h2 id="sy-arch-title" className="sy-section-title">
          <Translate id="home.arch.title">Four lanes, one unified front door</Translate>
        </h2>
        <p className="sy-section-lead">
          <Translate id="home.arch.lead">
            Every request passes input and output guardrails, then routes through the standard OpenAI lane, the native Vertex lane, the Jev decision lane, or the 2D UHP harness lane. Each route is an ordered fallback chain, and every call lands in the cost ledger and the metrics pipeline.
          </Translate>{' '}
          <Link to="/docs/overview/architecture/">
            <Translate id="home.arch.more">Read the architecture guide</Translate>
          </Link>
        </p>
        <div className={styles.scroller} role="region" aria-labelledby="sy-arch-title" tabIndex={0}>
          <svg viewBox="0 0 800 430" className={styles.diagram} role="img" aria-label={translate({id: 'home.arch.aria', message: 'Synapse request flow across four lanes'})}>
            <defs>
              <marker id="sy-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
                <path d="M0 0 L10 5 L0 10 z" className={styles.arrowHead} />
              </marker>
            </defs>
            <DiagramNode x={10} y={156} w={90} label={translate({id: 'home.arch.client', message: 'Client'})} />
            <DiagramNode x={130} y={156} w={110} label={translate({id: 'home.arch.guardrails', message: 'Guardrails'})} />
            <DiagramNode x={270} y={156} w={140} label={translate({id: 'home.arch.detect', message: 'Lane & Jev router'})} />
            <Arrow d="M100 184 H128" />
            <Arrow d="M240 184 H268" />
            {lanes.map((lane) => (
              <g key={lane.y}>
                <Arrow d={`M410 184 C425 184 425 ${lane.y + 28} 438 ${lane.y + 28}`} />
                <DiagramNode x={440} y={lane.y} w={160} label={lane.label} accent={lane.accent} />
                <Arrow d={`M600 ${lane.y + 28} H628`} />
                <DiagramNode x={630} y={lane.y} w={160} label={lane.provider} />
              </g>
            ))}
            <rect x={10} y={372} width={780} height={40} rx={8} className={styles.bar} />
            <text x={400} y={397} textAnchor="middle" className={styles.barLabel}>
              {translate({id: 'home.arch.bar', message: '1D & 2D Jev routing · Fallback chains · Input & output guardrails · Cost ledger · OpenTelemetry'})}
            </text>
          </svg>
        </div>
      </div>
    </section>
  );
}
