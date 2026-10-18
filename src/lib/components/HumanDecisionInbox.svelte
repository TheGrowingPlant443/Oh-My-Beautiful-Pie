<script lang="ts">
  import type { HumanDecision, HumanDecisionResponse, HumanDecisionTarget, NativeDecisionQuestion } from "../api";
  import { askResponse, canAnswer, decisionKey, humanDecisions, nativeOption, readDecisionImage, toggleQuestionOption, type QuestionDraft } from "../human-decisions";
  import { allInstances } from "../stores";

  let { onOpenSession }: { onOpenSession?: (id: string) => void } = $props();
  const unavailable = $derived($allInstances.filter(instance => instance.status === "running" && instance.capabilities?.nativeDecisions !== true));
  const escalations = $derived($allInstances.filter(instance => instance.humanRequest && !instance.humanRequest.decisionId));
  function setText(target: HumanDecisionTarget, value: string): void {
    humanDecisions.updateDraft(target, draft => ({ ...draft, text: value }));
  }
  function setQuestionText(target: HumanDecisionTarget, id: string, field: "customInput" | "note", value: string): void {
    questionDraft(target, id, draft => ({ ...draft, [field]: value }));
  }
  function questionDraft(target: HumanDecisionTarget, id: string, update: (draft: QuestionDraft) => QuestionDraft): void {
    humanDecisions.updateDraft(target, draft => ({ ...draft, questions: { ...draft.questions, [id]: update(draft.questions[id]) } }));
  }
  function toggle(item: HumanDecision, question: NativeDecisionQuestion, index: number): void {
    questionDraft(item.target, question.id, draft => toggleQuestionOption(question, draft, index));
  }
  function submit(item: HumanDecision, response?: HumanDecisionResponse): void {
    try {
      const draft = $humanDecisions.drafts.get(decisionKey(item.target));
      if (!draft || draft.loadingImages) return;
      void humanDecisions.submit(item.target, response ?? (item.method === "askDialog" ? askResponse(item, draft) : draft.text));
    } catch (error) {
      humanDecisions.updateDraft(item.target, draft => ({ ...draft, error: String(error) }));
    }
  }
  async function attachImages(item: HumanDecision, questionId: string, field: "customInputImages" | "noteImages", input: HTMLInputElement): Promise<void> {
    const files = Array.from(input.files ?? []);
    input.value = "";
    if (!item.payload.acceptImages || !files.length) return;
    humanDecisions.updateDraft(item.target, draft => ({ ...draft, loadingImages: draft.loadingImages + 1, error: null }));
    try {
      const images = await Promise.all(files.map(readDecisionImage));
      questionDraft(item.target, questionId, draft => ({ ...draft, [field]: [...draft[field], ...images] }));
    } catch (error) {
      humanDecisions.updateDraft(item.target, draft => ({ ...draft, error: `Image not attached: ${String(error)}` }));
    } finally {
      humanDecisions.updateDraft(item.target, draft => ({ ...draft, loadingImages: draft.loadingImages - 1 }));
    }
  }
</script>

<section class="inbox" aria-label="Human decisions">
  <div class="heading"><h3>Decisions <span class="pill">{$humanDecisions.items.length}</span></h3><button type="button" onclick={() => humanDecisions.refresh()}>Refresh</button></div>
  {#if $humanDecisions.error}<p class="err" role="alert">{$humanDecisions.error}</p>{/if}
  {#if $humanDecisions.loading}<p role="status">Loading decisions…</p>
  {:else if !$humanDecisions.items.length && !unavailable.length && !escalations.length && !$humanDecisions.error}<p class="empty">No agents are waiting for an answer</p>{/if}
  {#each unavailable as instance (instance.id)}
    <article class="card"><h4>{instance.name}</h4><p class="err">{instance.capabilities?.nativeDecisionsError ?? "Native decision capture is unavailable. Reopen this session with a compatible OMP version; pending native questions cannot be listed reliably."}</p>{#if onOpenSession}<button type="button" onclick={() => onOpenSession?.(instance.id)}>Open session</button>{/if}</article>
  {/each}
  {#each escalations as instance (instance.id)}
    <article class="card"><h4>{instance.name} needs you</h4><pre>{instance.humanRequest?.reason}</pre><p class="context">Open the session to discuss this request or return control. This is not a native approval question.</p>{#if onOpenSession}<button type="button" onclick={() => onOpenSession?.(instance.id)}>Open session</button>{/if}</article>
  {/each}
  {#each $humanDecisions.items as item (decisionKey(item.target))}
    {@const key = decisionKey(item.target)}
    {@const draft = $humanDecisions.drafts.get(key)!}
    {@const busy = draft.submitting || draft.awaitingResolution || draft.loadingImages > 0}
    <article class="card">
      <header><strong>{item.instanceName}</strong><span class="pill">{item.instanceKind}</span><span class="pill">{item.method}</span>{#if item.humanOnly}<span class="pill">Human only</span>{/if}<time datetime={new Date(item.createdAt).toISOString()}>{new Date(item.createdAt).toLocaleString()}</time></header>
      <p class="context">{item.agentOrigin?.name ?? item.agentOrigin?.kind ?? "Unattributed native request"}{#if item.agentOrigin?.id} · {item.agentOrigin.id}{/if}</p>
      <p class="context"><strong>{item.repository.label}</strong> · {item.repository.scope}</p>
      {#if item.repository.folder}<p class="path">{item.repository.folder}</p>{/if}
      {#if item.payload.cwd && item.payload.cwd !== item.repository.folder}<p class="path">Working folder: {item.payload.cwd}</p>{/if}
      {#if item.payload.title}<h4>{item.payload.title}</h4>{/if}
      {#if item.payload.prompt}<pre class="prompt">{item.payload.prompt.full}</pre>{#if item.payload.prompt.compact !== item.payload.prompt.full}<details><summary>Compact native prompt</summary><pre>{item.payload.prompt.compact}</pre></details>{/if}{/if}
      {#if item.payload.message}<pre class="prompt">{item.payload.message}</pre>{/if}
      {#if item.payload.helpText}<p>{item.payload.helpText}</p>{/if}
      {#if item.payload.timeout !== undefined}<p class="context">Native timeout: {item.payload.timeout} ms. The native session may resolve this request before you answer.</p>{/if}
      {#if canAnswer(item)}
        <fieldset disabled={busy} class="answer">
          {#if item.method === "approve" || item.method === "confirm"}
            <div class="actions"><button type="button" class="primary" onclick={() => submit(item, true)}>Approve</button><button type="button" onclick={() => submit(item, false)}>Deny</button></div>
          {:else if item.method === "select"}
            <div class="options">{#each item.payload.options ?? [] as rawOption, index}
              {@const option = nativeOption(rawOption)}
              <div class="option"><button type="button" disabled={item.payload.disabledIndices?.includes(index)} onclick={() => submit(item, option.label)}>{option.label}</button>{#if option.description}<p>{option.description}</p>{/if}{#if option.preview}<pre>{option.preview}</pre>{/if}</div>
            {/each}</div>
          {:else if item.method === "input" || item.method === "editor"}
            <label>Answer<textarea rows={item.method === "editor" ? 8 : 3} placeholder={item.payload.placeholder ?? ""} value={draft.text} oninput={event => setText(item.target, event.currentTarget.value)}></textarea></label>
            <button type="button" class="primary" onclick={() => submit(item)}>Submit answer</button>
          {:else if item.method === "askDialog"}
            {#each item.payload.questions ?? [] as question (question.id)}
              {@const answer = draft.questions[question.id]}
              <fieldset class="question"><legend>{question.question}</legend><p class="context">{question.multi ? "Choose any number of options" : "Choose up to one option"} · {question.id}</p>
                {#each question.options as rawOption, index}
                  {@const option = nativeOption(rawOption)}
                  <div class="option"><label class="choice"><input type={question.multi ? "checkbox" : "radio"} name={`${key}:${question.id}`} checked={answer.selectedOptions.includes(option.label)} disabled={question.disabledIndices?.includes(index)} onchange={() => toggle(item, question, index)} /><span>{option.label}{#if index === question.recommended} <span class="pill">Recommended</span>{/if}</span></label>{#if option.description}<p>{option.description}</p>{/if}{#if option.preview}<pre>{option.preview}</pre>{/if}</div>
                {/each}
                <button type="button" class="ghost" onclick={() => questionDraft(item.target, question.id, draft => ({ ...draft, selectedOptions: [] }))}>Clear selection</button>
                <label>Custom answer<textarea rows="2" value={answer.customInput} oninput={event => setQuestionText(item.target, question.id, "customInput", event.currentTarget.value)}></textarea></label>
                <label>Note<textarea rows="2" value={answer.note} oninput={event => setQuestionText(item.target, question.id, "note", event.currentTarget.value)}></textarea></label>
                {#if item.payload.acceptImages}
                  {#each ["customInputImages", "noteImages"] as imageField}
                    {@const field = imageField as "customInputImages" | "noteImages"}
                    <label>{field === "customInputImages" ? "Custom answer images" : "Note images"}<input type="file" accept="image/*" multiple onchange={event => attachImages(item, question.id, field, event.currentTarget)} /></label>
                    <div class="attachments">{#each answer[field] as image, index}<div><img alt={`Selected ${field === "customInputImages" ? "answer" : "note"} attachment ${index + 1}`} src={`data:${image.mimeType};base64,${image.data}`} /><button type="button" onclick={() => questionDraft(item.target, question.id, draft => ({ ...draft, [field]: draft[field].filter((_, at) => at !== index) }))}>Remove image {index + 1}</button></div>{/each}</div>
                  {/each}
                {/if}
              </fieldset>
            {/each}
            <div class="actions"><button type="button" class="primary" onclick={() => submit(item)}>Submit answers</button>{#if item.payload.supportsChat}<button type="button" onclick={() => submit(item, { kind: "chat" })}>Chat</button>{/if}</div>
          {/if}
          <button type="button" class="ghost" onclick={() => submit(item, { cancelled: true })}>Cancel</button>
        </fieldset>
      {:else}
        <p class="context">This request must be handled in the native session.</p>
      {/if}
      {#if onOpenSession}<button type="button" onclick={() => onOpenSession?.(item.target.instanceId)}>Open session</button>{/if}
      {#if draft.submitting}<p role="status">Submitting answer…</p>{:else if draft.awaitingResolution}<p role="status">Answer accepted. Waiting for native resolution…</p>{/if}
      {#if draft.loadingImages}<p role="status">Reading selected images…</p>{/if}
      {#if draft.error}<p class="err" role="alert">{draft.error}</p>{/if}
    </article>
  {/each}
</section>

<style>
  .inbox { height: 100%; overflow: auto; padding: 10px; box-sizing: border-box; }
  .heading, header, .actions { display: flex; flex-wrap: wrap; align-items: center; gap: 8px; }
  .heading { justify-content: space-between; }
  h3, h4, p { margin: 6px 0; }
  .card { border: 1px solid var(--line); border-radius: 8px; padding: 14px; margin-top: 12px; background: #151e2c; }
  time { margin-left: auto; font-size: 12px; color: var(--muted); }
  .context { color: var(--muted); font-size: 12px; }
  .path, p, legend { overflow-wrap: anywhere; }
  pre { white-space: pre-wrap; overflow-wrap: anywhere; font: inherit; margin: 8px 0; }
  .prompt { padding: 10px; background: #101722; border-radius: 5px; }
  fieldset { min-width: 0; }
  .answer { border: 0; padding: 0; display: grid; justify-items: start; gap: 10px; margin: 12px 0; }
  .answer > label, .question, .options { width: 100%; box-sizing: border-box; }
  .question { border: 1px solid var(--line); padding: 12px; }
  label { display: grid; gap: 5px; margin: 7px 0; }
  textarea { box-sizing: border-box; width: 100%; resize: vertical; }
  .choice { display: flex; align-items: baseline; gap: 8px; }
  .choice input { width: auto; flex: none; }
  .option { margin: 6px 0; padding: 7px; border-bottom: 1px solid var(--line); }
  .option p, .option pre { margin-left: 6px; }
  .attachments { display: flex; flex-wrap: wrap; gap: 8px; }
  .attachments > div { display: grid; gap: 4px; }
  img { width: 96px; height: 72px; object-fit: contain; }
  .empty { padding: 24px 0; }
</style>
