<?php

namespace Tenma\Discord;

/**
 * What a bot sends: text, embeds and buttons. A plain string does for text alone. When it edits a message,
 * what it leaves null stays as it was, and an empty list removes the embeds or buttons.
 *
 *     new Message('Now playing', embeds: [new Embed(title: 'listen.moe')], buttons: [new Button('stop', 'Stop')])
 *
 * Mentions in the content ping nobody, so echoed user text cannot reach @everyone. `mentions` lets them through,
 * as Discord's allowed_mentions: `['users' => [$id]]` pings that user, `['parse' => ['users', 'roles']]` every
 * mentioned user and role, `['parse' => ['everyone']]` also @everyone and @here.
 */
final readonly class Message
{
    /**
     * @param list<Embed>|null $embeds
     * @param list<Button>|null $buttons up to 25, five to a row
     * @param bool $ephemeral only the user of an interaction sees it
     * @param array{
     *     parse?: list<'users'|'roles'|'everyone'>,
     *     users?: list<string>,
     *     roles?: list<string>,
     *     replied_user?: bool,
     * } $mentions who the content may ping; nobody by default
     */
    public function __construct(
        public ?string $content = null,
        public ?array $embeds = null,
        public ?array $buttons = null,
        public bool $ephemeral = false,
        public array $mentions = [],
    ) {}

    public static function from(string|self $message): self
    {
        return is_string($message) ? new self($message) : $message;
    }

    /** @return array<string, mixed> */
    public function toArray(): array
    {
        $message = ['flags' => $this->ephemeral ? 1 << 6 : 0];

        if ($this->content !== null) {
            $message['content'] = $this->content;
        }
        if ($this->embeds !== null) {
            $message['embeds'] = array_map(fn (Embed $embed) => $embed->toArray(), $this->embeds);
        }
        if ($this->buttons !== null) {
            $message['components'] = self::rows($this->buttons);
        }
        $message['allowed_mentions'] = $this->mentions + ['parse' => []];

        return $message;
    }

    /**
     * @param list<Button> $buttons
     * @return list<array{type: int, components: list<array<string, mixed>>}>
     */
    private static function rows(array $buttons): array
    {
        $rows = [];
        foreach (array_chunk($buttons, 5) as $row) {
            $rows[] = [
                'type' => 1,
                'components' => array_map(fn (Button $button) => $button->toArray(), $row),
            ];
        }

        return $rows;
    }
}
