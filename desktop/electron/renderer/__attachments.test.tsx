/// <reference types="vitest/globals" />
import { describe, it, expect, vi, beforeEach } from 'vitest';
import { fireEvent, render, screen, waitFor } from '@testing-library/react';
import { saveAttachments, UserMessageContent, AssistantContent, Composer, FeedbackRow, imagesAcceptedForRoute } from './app';
import { I18nProvider } from './i18n';

declare global {
  interface Window {
    starriver: any;
  }
}

beforeEach(() => {
  window.starriver = {
    admitImageAttachments: vi.fn(),
    saveTempFile: vi.fn(),
  };
});

describe('saveAttachments', () => {
  it('admits images through the attachment seam and builds sha256 refs', async () => {
    window.starriver.admitImageAttachments.mockResolvedValue([
      { id: 'sha256:abc', mediaType: 'image/jpeg', width: 64, height: 32, bytes: 100 },
    ]);
    const { saved, imageRefs, docLinks, docTexts } = await saveAttachments([
      {
        id: 'a1',
        name: 'photo.png',
        kind: 'image',
        dataUrl: 'data:image/png;base64,AAAA',
        state: 'pending',
      },
    ]);
    expect(window.starriver.admitImageAttachments).toHaveBeenCalledTimes(1);
    expect(imageRefs).toBe('![photo.png](sha256:abc)');
    expect(saved[0].state).toBe('saved');
    expect(saved[0].ref).toBe('sha256:abc');
    expect(docLinks).toBe('');
    expect(docTexts).toBe('');
  });

  it('keeps binary documents on the temp-file path', async () => {
    window.starriver.saveTempFile.mockResolvedValue('/tmp/x.pdf');
    const { docLinks } = await saveAttachments([
      {
        id: 'a1',
        name: 'doc.pdf',
        kind: 'document',
        dataUrl: 'data:application/pdf;base64,AAAA',
        state: 'pending',
      },
    ]);
    expect(docLinks).toBe('[doc.pdf](/tmp/x.pdf)');
    expect(window.starriver.admitImageAttachments).not.toHaveBeenCalled();
  });

  it('marks images as error when admission fails', async () => {
    window.starriver.admitImageAttachments.mockRejectedValue(new Error('too large'));
    const { saved, imageRefs } = await saveAttachments([
      {
        id: 'a1',
        name: 'big.png',
        kind: 'image',
        dataUrl: 'data:image/png;base64,AAAA',
        state: 'pending',
      },
    ]);
    expect(saved[0].state).toBe('error');
    expect(imageRefs).toBe('');
  });

  it('reuses durable refs for already-admitted images', async () => {
    const { imageRefs, saved } = await saveAttachments([
      {
        id: 'a1',
        name: 'photo.png',
        kind: 'image',
        dataUrl: 'data:image/png;base64,AAAA',
        ref: 'sha256:existing',
        state: 'saved',
      },
    ]);
    expect(imageRefs).toBe('![photo.png](sha256:existing)');
    expect(saved[0].state).toBe('saved');
    expect(window.starriver.admitImageAttachments).not.toHaveBeenCalled();
  });
});

describe('UserMessageContent image refs (WI-1 P1)', () => {
  it('renders sha256 refs as thumbnails via attachments.read', async () => {
    window.starriver.readImageAttachment = vi.fn().mockResolvedValue({
      id: 'sha256:abc',
      mediaType: 'image/png',
      width: 8,
      height: 8,
      base64: 'AAAA',
    });
    const { container } = render(
      <UserMessageContent text={'look ![图片 image/png](sha256:abc) done'} />,
    );
    expect(screen.getByText(/look/)).toBeTruthy();
    await waitFor(() => expect(container.querySelector('img')).toBeTruthy());
    const img = container.querySelector('img') as HTMLImageElement;
    expect(img.src).toBe('data:image/png;base64,AAAA');
    expect(window.starriver.readImageAttachment).toHaveBeenCalledWith('sha256:abc');
  });

  it('falls back to a placeholder when the read fails', async () => {
    window.starriver.readImageAttachment = vi.fn().mockRejectedValue(new Error('gone'));
    const { container, getByText } = render(
      <UserMessageContent text={'![图片 image/png](sha256:missing)'} />,
    );
    await waitFor(() => expect(getByText(/🖼/)).toBeTruthy());
    expect(container.querySelector('img')).toBeNull();
  });

  it('renders data urls directly without fetching', () => {
    window.starriver.readImageAttachment = vi.fn();
    const { container } = render(
      <UserMessageContent text={'![x](data:image/png;base64,BBBB)'} />,
    );
    const img = container.querySelector('img') as HTMLImageElement;
    expect(img).toBeTruthy();
    expect(img.src).toContain('data:image/png;base64,BBBB');
    expect(window.starriver.readImageAttachment).not.toHaveBeenCalled();
  });
});

describe('AssistantContent image refs (WI-102 generated images)', () => {
  it('renders sha256 refs echoed by the assistant as thumbnails via attachments.read', async () => {
    window.starriver.readImageAttachment = vi.fn().mockResolvedValue({
      id: 'sha256:gen',
      mediaType: 'image/png',
      width: 8,
      height: 8,
      base64: 'AAAA',
    });
    const { container } = render(
      <AssistantContent content={'Generated:\n![generated-1](sha256:gen)\n(saved: /tmp/x.png)'} />,
    );
    await waitFor(() => expect(container.querySelector('img')).toBeTruthy());
    const img = container.querySelector('img') as HTMLImageElement;
    expect(img.src).toBe('data:image/png;base64,AAAA');
    expect(window.starriver.readImageAttachment).toHaveBeenCalledWith('sha256:gen');
  });

  it('renders data urls inline and leaves http sources native', () => {
    window.starriver.readImageAttachment = vi.fn();
    const { container } = render(
      <AssistantContent content={'![a](data:image/png;base64,DDDD) and ![b](https://example.com/x.png)'} />,
    );
    const imgs = container.querySelectorAll('img');
    expect(imgs.length).toBe(2);
    expect((imgs[0] as HTMLImageElement).src).toContain('data:image/png;base64,DDDD');
    expect((imgs[1] as HTMLImageElement).src).toBe('https://example.com/x.png');
    expect(window.starriver.readImageAttachment).not.toHaveBeenCalled();
  });
});

describe('Composer @file completion (WI-7)', () => {
  function renderComposer(onChange = vi.fn(), attachments: any[] = []) {
    const ref = { current: null } as unknown as React.RefObject<HTMLTextAreaElement>;
    const view = render(
      <I18nProvider>
        <Composer
          value={''}
          onChange={onChange}
          onKeyDown={() => {}}
          onPaste={() => {}}
          onDrop={() => {}}
          placeholder=""
          disabled={false}
          inputRef={ref}
          attachments={attachments}
          onRemoveAttachment={() => {}}
          onFileSelect={() => {}}
          onSend={() => {}}
          onStop={() => {}}
          canSend={true}
          fileSuggest={{ workspaceId: 'w1' }}
        />
      </I18nProvider>,
    );
    return { container: view.container, onChange };
  }

  it('renders pending image attachments as real previews inside the input bar', () => {
    const { container } = renderComposer(undefined, [
      { id: 'a1', name: 'image.png', kind: 'image', dataUrl: 'data:image/png;base64,AAAA', state: 'pending' },
    ]);
    // The strip lives INSIDE the input box (own row above the textarea), and
    // the thumb is the actual image (scaled-down preview), not a name chip.
    const strip = container.querySelector('.input-bar .attachment-strip') as HTMLElement;
    expect(strip).toBeTruthy();
    const img = strip.querySelector('.attachment-thumb img') as HTMLImageElement;
    expect(img).toBeTruthy();
    expect(img.getAttribute('src')).toBe('data:image/png;base64,AAAA');
  });

  it('opens a lightbox when the image thumb is clicked and closes via the button', () => {
    const { container } = renderComposer(undefined, [
      { id: 'a1', name: 'image.png', kind: 'image', dataUrl: 'data:image/png;base64,AAAA', state: 'pending' },
    ]);
    // No in-flight "…" overlay on pasted images — the thumb is a preview.
    expect(container.querySelector('.thumb-overlay')).toBeNull();
    fireEvent.click(container.querySelector('.attachment-thumb') as HTMLElement);
    const box = container.querySelector('.img-lightbox') as HTMLElement;
    expect(box).toBeTruthy();
    expect((box.querySelector('.img-lightbox-img') as HTMLImageElement).getAttribute('src')).toBe('data:image/png;base64,AAAA');
    fireEvent.click(box.querySelector('.img-lightbox-close') as HTMLElement);
    expect(container.querySelector('.img-lightbox')).toBeNull();
  });

  it('opens on @ and lists workspace files', async () => {
    window.starriver.suggestFiles = vi.fn().mockResolvedValue(['src/main.rs', 'docs/readme.md']);
    const { container } = renderComposer();
    const ta = container.querySelector('textarea') as HTMLTextAreaElement;
    fireEvent.change(ta, { target: { value: '@' } });
    await waitFor(() => expect(screen.getByText('@src/main.rs')).toBeTruthy());
    expect(window.starriver.suggestFiles).toHaveBeenCalledWith('w1', '');
  });

  it('inserts the selected path on click', async () => {
    window.starriver.suggestFiles = vi.fn().mockResolvedValue(['src/main.rs']);
    const onChange = vi.fn();
    const { container } = renderComposer(onChange);
    const ta = container.querySelector('textarea') as HTMLTextAreaElement;
    fireEvent.change(ta, { target: { value: '@' } });
    await waitFor(() => expect(screen.getByText('@src/main.rs')).toBeTruthy());
    fireEvent.mouseDown(screen.getByText('@src/main.rs'));
    expect(onChange).toHaveBeenLastCalledWith('@src/main.rs ');
  });
});

describe('FeedbackRow (WI-48 feedback slice)', () => {
  it('records 👍 immediately', async () => {
    const onRecord = vi.fn();
    render(
      <I18nProvider>
        <FeedbackRow onRecord={onRecord} />
      </I18nProvider>,
    );
    fireEvent.click(screen.getByLabelText('Helpful'));
    expect(onRecord).toHaveBeenCalledWith('up', undefined);
    expect(screen.getByText('Thanks — feedback recorded.')).toBeTruthy();
  });

  it('👎 opens a comment input and submits with it', () => {
    const onRecord = vi.fn();
    render(
      <I18nProvider>
        <FeedbackRow onRecord={onRecord} />
      </I18nProvider>,
    );
    fireEvent.click(screen.getByLabelText('Needs work'));
    fireEvent.change(screen.getByPlaceholderText('What was wrong? (optional)'), {
      target: { value: 'wrong file' },
    });
    fireEvent.click(screen.getByText('Send feedback'));
    expect(onRecord).toHaveBeenCalledWith('down', 'wrong file');
  });
});

describe('WI-152 S2 image gate', () => {
  const provider = (id: string, visionSupported?: boolean): any => ({
    id, name: id, modelType: 'cloud', providerType: 'openai-compat', baseUrl: 'https://x',
    apiKey: 'k', models: [{ id: 'm1', name: 'M1' }], defaultModel: 'm1',
    apiFormat: 'openai-chat', supportsVision: visionSupported === undefined ? null : visionSupported,
    visionSupported,
  });

  it('imagesAcceptedForRoute resolves the active provider and defaults permissive', () => {
    const models = [
      provider('deepseek', true),
      provider('moonshot', false),
    ];
    expect(imagesAcceptedForRoute(models, 'deepseek/m1')).toBe(true);
    expect(imagesAcceptedForRoute(models, 'moonshot/moonshot-v1-8k')).toBe(false);
    // Unknown provider (self-hosted endpoint) stays permissive.
    expect(imagesAcceptedForRoute(models, 'my-relay/chat')).toBe(true);
    expect(imagesAcceptedForRoute([], 'deepseek/m1')).toBe(true);
  });

  it('Composer excludes image types from the picker and renders the gate notice', () => {
    const { container } = render(
      <I18nProvider>
        <Composer
          value="" onChange={() => {}} onKeyDown={() => {}} onPaste={() => {}}
          onDrop={() => {}} placeholder="msg" disabled={false}
          inputRef={{ current: null } as any}
          attachments={[]} onRemoveAttachment={() => {}} onFileSelect={() => {}}
          imagesDisabled imagesNotice="当前模型不支持图片输入"
          onSend={() => {}} onStop={() => {}} canSend
        />
      </I18nProvider>,
    );
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    expect(input.accept).not.toContain('image/*');
    const alert = container.querySelector('[role="alert"]');
    expect(alert?.textContent).toContain('当前模型不支持图片输入');
  });

  it('Composer keeps image types in the picker when the route accepts images', () => {
    const { container } = render(
      <I18nProvider>
        <Composer
          value="" onChange={() => {}} onKeyDown={() => {}} onPaste={() => {}}
          onDrop={() => {}} placeholder="msg" disabled={false}
          inputRef={{ current: null } as any}
          attachments={[]} onRemoveAttachment={() => {}} onFileSelect={() => {}}
          onSend={() => {}} onStop={() => {}} canSend
        />
      </I18nProvider>,
    );
    const input = container.querySelector('input[type="file"]') as HTMLInputElement;
    expect(input.accept).toContain('image/*');
    expect(container.querySelector('[role="alert"]')).toBeNull();
  });
});
