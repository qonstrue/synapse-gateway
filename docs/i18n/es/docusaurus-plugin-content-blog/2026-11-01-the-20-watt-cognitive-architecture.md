---
slug: the-20-watt-cognitive-architecture
title: "La arquitectura cognitiva de 20 vatios: hacia la AGI con modelos de decisión de Sistema 1 y habilidades reutilizables"
authors: [rajwilkhu]
tags: [routing, tooling]
date: 2026-11-01T17:00
description: Por qué escalar hacia la AGI exige invertir la pila actual basada únicamente en el Sistema 2, combinando modelos de decisión de Sistema 1 en menos de 100 ms (TypeSafe Jev), habilidades de agente reutilizables y escalado controlado al Sistema 2 para acercarse a la eficiencia de 20 vatios de la inteligencia biológica.
---

La infraestructura moderna de inteligencia artificial consume kilovatios de energía por bastidor
para generar tokens de forma secuencial, palabra por palabra. Sin embargo, como señala el
neurocientífico Henning Beck, el cerebro humano opera con apenas **20 vatios de potencia** —apenas
lo suficiente para encender una bombilla tenue— y aun así abarca todo el espectro de la percepción,
el control motor, el aprendizaje abstracto y el razonamiento creativo.

¿Cómo logra la inteligencia biológica esa versatilidad sin colapsar bajo su presupuesto energético?
No calcula cada acción desde los primeros principios. En su lugar, el cerebro funciona como una
**máquina de intervención por defecto**: compara continuamente el estado entrante con un repertorio
de **habilidades** (*skills*) aprendidas, activando heurísticas rápidas y de bajo consumo para las
tareas rutinarias y reservando la deliberación consciente —intensiva en energía— estrictamente para
desafíos novedosos o fuera de distribución.

Si los agentes de software autónomos van a escalar hacia la AGI sin chocar contra un muro de
latencia y energía, nuestra infraestructura debe adoptar esa misma arquitectura de doble sistema.

<!-- truncate -->

## Construimos la pila cognitiva al revés

En psicología cognitiva, Daniel Kahneman describió dos modos de pensamiento:

- **Sistema 1**: rápido, instintivo, paralelo, automático y de consumo energético ultrabajo.
- **Sistema 2**: lento, deliberado, secuencial, lógico y metabólicamente costoso.

Como destaca el investigador Georgije Stanisic en su análisis de la arquitectura hacia la AGI, la IA
moderna ha construido esta pila cognitiva al revés. Casi todos los agentes en producción hoy en día
enrutan el 100 % de sus ciclos operativos a través de grandes modelos de lenguaje (LLM) que actúan
como razonadores de Sistema 2.

Invocar un LLM de frontera de más de 70 mil millones de parámetros para clasificar un estado,
enrutar una intención, evaluar una barrera de seguridad o elegir una herramienta es el equivalente
computacional de contratar a un comité de catedráticos para encender un interruptor de luz.
Introduce segundos de latencia, dispara el gasto de tokens y añade fragilidad probabilística (claves
JSON alucinadas o esquemas desviados) exactamente en la capa de la pila que debería ser un reflejo.

| Atributo cognitivo | Sistema 1 (Modelos de decisión / Jev) | Sistema 2 (LLM deliberativos) |
|---|---|---|
| **Función principal** | Reflejo intuitivo rápido y decisiones acotadas por esquema | Deliberación profunda, síntesis compleja y generación |
| **Velocidad de ejecución** | Menos de 100 ms (`70–500 ms` con muestreo paralelo) | Generación secuencial de tokens de varios segundos |
| **Costo energético y de cómputo** | Energía ultrabaja (eficiencia alineada con los 20 vatios) | Alta huella de cómputo y electricidad |
| **Espacio de salida** | Probabilidades estructuradas con seguridad de tipos (sin texto libre) | Texto en lenguaje natural y código sin restricciones |
| **Riesgo de alucinación** | Cero (acotado por validación estricta de esquema) | Variable (requiere barreras de seguridad y verificación externas) |
| **Rol en la pila AGI** | Filtro operativo por defecto, enrutador y compuerta de seguridad | Respaldo de escalado para razonamiento novedoso y no rutinario |

## Modelos de decisión de Sistema 1: la mecánica de Jev

Para llevar el Sistema 1 a los agentes de software, TypeSafe AI presentó **Jev**, un **modelo
System One** diseñado para *decidir* en lugar de redactar prosa. En vez de muestrear tokens de
forma autorregresiva para ojos humanos, un modelo de decisión de Sistema 1 evalúa un estado acotado
frente a preguntas tipadas (`choice`, `score`, `noul`) y devuelve distribuciones de probabilidad
legibles por máquina y epistémicamente calibradas:

1. **Muestreo paralelo de decisiones**: En lugar de generar un párrafo token por token, Jev evalúa
   múltiples preguntas estructuradas en paralelo en una sola pasada, hasta dos órdenes de magnitud
   más rápido que un turno deliberativo de un LLM.
2. **Cero alucinaciones de esquema**: Como el espacio de salida está estrictamente restringido a
   los tipos de pregunta declarados (opciones `choice`, criterios ordinales `score` o probabilidades
   `noul` en `[0, 1]`), el modelo no puede inventar nombres de campos, emitir JSON malformado ni
   salirse del esquema.
3. **Confianza epistémicamente calibrada**: Cada decisión incluye un puntaje de confianza calibrado,
   lo que brinda a los sistemas un umbral determinista (`min_confidence`) para saber *cuándo sabe
   el Sistema 1* y *cuándo debe escalar al Sistema 2*.
4. **Flujo agéntico ininterrumpido**: Al igual que las arquitecturas multimodales en tiempo real
   (como Gemini Live Avatar de Google) dependen de un despacho rápido en segundo plano para
   ejecutar herramientas sin interrumpir el diálogo, las decisiones de Sistema 1 en menos de 100 ms
   mantienen ágil el bucle de control.

## Habilidades aprendidas y arneses agénticos

En el software, los patrones motores y cognitivos aprendidos por el cerebro biológico adoptan la
forma de **habilidades** (*skills*): flujos de trabajo componibles y deterministas, definiciones de
herramientas y arneses de ejecución (`pi`, `claude-code`, `codex`, `hermes`, `systemone`) que
estructuran cómo interactúa un agente con su entorno.

En lugar de lanzar un LLM de frontera a un bucle sin restricciones esperando que redescubra el
procedimiento adecuado desde cero en cada turno, una arquitectura cognitiva de doble sistema
combina **enrutamiento de Sistema 1**, **habilidades reutilizables** y **escalado controlado al
Sistema 2** en cuatro etapas:

```
┌────────────────────────┐
│ 1. Ingesta rápida      │  El estado multimodal / UHP llega a Synapse
└───────────┬────────────┘
            ▼
┌────────────────────────┐
│ 2. Evaluación Sistema 1│  Jev evalúa el estado frente a habilidades, perfiles y niveles (<100 ms)
│    (TypeSafe Jev)      │  ──► ¿Alta confianza y rutinario? Ejecuta habilidad en nivel rápido
└───────────┬────────────┘
            │ Ambiguo, novedoso o de alta complejidad
            ▼
┌────────────────────────┐
│ 3. Deliberación        │  Escala al nivel de razonamiento profundo + arnés de ingeniería
│    Sistema 2           │  con mayor esfuerzo y horizonte de pasos
└───────────┬────────────┘
            ▼
┌────────────────────────┐
│ 4. Bucle de            │  Destila la solución novedosa del Sistema 2 en una habilidad reutilizable,
│    habilidades         │  ampliando el repertorio de bajo consumo del Sistema 1
└────────────────────────┘
```

1. **Ingesta de alta velocidad**: El estado entrante —ya sea un turno de chat OpenAI, un flujo
   multimodal o una tarea del Protocolo Universal de Arneses (`POST /v1/responses`)— llega al
   gateway.
2. **Emparejamiento de habilidades y arneses con el Sistema 1**: Antes de despertar un modelo
   pesado, Synapse consulta a Jev para filtrar barreras de seguridad
   ([clasificadores de guardrails](/docs/configuration/guardrails-policy/)) y puntuar la solicitud
   según los perfiles de arquetipo de trabajo y los niveles de dificultad
   ([enrutador Jev 1D](/docs/guides/jev-router/) y
   [enrutador Jev 2D de arneses](/docs/guides/jev-harness-router/)). Cuando la tarea coincide con
   habilidades conocidas con alta confianza, se ejecuta de inmediato en un ejecutor ligero y un
   nivel rápido a una fracción de la energía y el costo.
3. **Escalado controlado al Sistema 2**: Cuando Jev detecta alta dificultad, baja confianza,
   necesidad de razonamiento paso a paso (`needs_reasoning`) o un horizonte largo de herramientas
   (`needs_deep_horizon`), Synapse escala el turno hacia un nivel de modelo de Sistema 2 de
   frontera con un presupuesto ampliado de razonamiento y límite de pasos.
4. **El bucle de retroalimentación de habilidades**: Una vez que el Sistema 2 resuelve un problema
   novedoso —depurar una condición de carrera desconocida, sintetizar un nuevo flujo de trabajo en
   el repositorio o verificar una migración de varios pasos—, esa solución se destila y empaqueta
   en una **habilidad** reutilizable. La próxima vez que aparezca un reto similar, el agente ya no
   necesita razonar desde cero: el Sistema 1 reconoce el patrón y despacha la habilidad
   directamente.

## Por qué esto importa en el camino hacia la AGI

El camino hacia la inteligencia autónoma de propósito general no se ganará convirtiendo cada acción
en un monólogo de cadena de pensamiento de 100 vatios y 10 segundos. Se ganará con sistemas que
**piensen rápido por defecto y piensen despacio bajo demanda**: utilizando modelos de decisión de
Sistema 1 en menos de 100 ms para proteger, enrutar y activar **habilidades** reutilizables en el
90 % de los turnos operativos, reservando la deliberación profunda del Sistema 2 para el 10 % de
los problemas donde ocurre la verdadera invención.

Esa es la arquitectura que estamos construyendo en Synapse: TypeSafe System One (Jev) como la capa
de reflejo rápido, `routes.toml` y `harnesses.toml` como la columna vertebral de escalado calibrado,
y arneses de agentes equipados con habilidades como las manos que convierten las decisiones en
acción.

## Referencias

- Beck, Henning. (2026). *The Brain Works with 20 Watts: Energy Efficiency and Human Cognition*. Munich Re Interview Series.
- Stanisic, Georgije. (2026). *Why Jev Is the Next Step to AGI*. Medium Research Series.
- TypeSafe AI Blog. (2026). *Introducing System One Models & Jev: AI That Decides Instead of Writing*.
- Google AI. (2026). *Introducing Gemini 3.8 Live with Live Avatar: Real-Time Multimodal Streaming & Asynchronous Tool Execution*.
- AGI Jetzt. (2026). *System One & Decision Models: Machine-Readable Probabilities in Programmatic Workflows*.
