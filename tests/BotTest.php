<?php

namespace Tenma\Discord\Tests;

use PHPUnit\Framework\TestCase;
use Tenma\Discord\{Bot, Button, Embed, Intents, Message, Modal, Option, TextInput};
use Tenma\Discord\Events\{Dispatch, MessageCreated, TrackFailed};
use Tenma\Discord\Interactions\{Autocomplete, Click, Command, Submit};

final class BotTest extends TestCase
{
    private FakeConnection $connection;
    private Bot $bot;

    protected function setUp(): void
    {
        $this->connection = new FakeConnection();
        $this->bot = new Bot('secret', Intents::Default, $this->connection);
    }

    private function connect(): void
    {
        $this->bot->start();
        $this->connection->emit('READY', [
            'user' => ['id' => '1', 'username' => 'tenma', 'bot' => true],
            'application' => ['id' => 'app'],
        ]);
    }

    /**
     * @param array<mixed> $data
     * @return array<mixed>
     */
    private static function interaction(int $type, array $data, string $user = 'u1'): array
    {
        return [
            'id' => 'i1',
            'token' => 't1',
            'type' => $type,
            'guild_id' => 'g1',
            'channel_id' => 'c1',
            'member' => ['user' => ['id' => $user, 'username' => 'lento']],
            'data' => $data,
            'message' => ['id' => 'm1'],
        ];
    }

    /** @return list<array<mixed>> */
    private function callbacks(): array
    {
        return $this->connection->rest('POST', '/interactions/i1/t1/callback');
    }

    /** @return array<mixed> the data of the first interaction callback */
    private function callbackData(): array
    {
        $data = $this->callbacks()[0]['data'] ?? null;

        return is_array($data) ? $data : [];
    }

    public function testStartRegistersSubscribesAndConnectsWithTheToken(): void
    {
        $this->bot->start();

        $functions = array_column($this->connection->calls, 'function');
        $this->assertSame(['register', 'start'], $functions);
        $this->assertSame(['token' => 'secret', 'intents' => Intents::Default], $this->connection->calls[1]['input']);
    }

    public function testReadySyncsCommandsDiscordDoesNotKnowYet(): void
    {
        $this->bot->command('ping', 'Answers with pong', fn (Command $command) => 'pong');

        $this->connect();

        $put = $this->connection->rest('PUT', '/applications/app/commands');
        $this->assertSame([[['name' => 'ping', 'description' => 'Answers with pong', 'options' => []]]], $put);
    }

    public function testReadyLeavesCommandsAloneThatDiscordAlreadyHas(): void
    {
        $this->bot->command('radio', 'Plays', fn (Command $command) => null, [Option::string('url', 'Stream')]);
        $this->connection->responses['GET /applications/app/commands'] = json_encode([[
            'id' => '9',
            'name' => 'radio',
            'description' => 'Plays',
            'options' => [['type' => 3, 'name' => 'url', 'description' => 'Stream']],
        ]], JSON_THROW_ON_ERROR);

        $this->connect();

        $this->assertSame([], $this->connection->rest('PUT', '/applications/app/commands'));
    }

    public function testOnlyTheFirstWorkerSyncsCommands(): void
    {
        $connection = new FakeConnection('discord.w2');
        $bot = new Bot('secret', Intents::Default, $connection);
        $bot->command('ping', 'Answers with pong', fn (Command $command) => 'pong');

        $bot->start();
        $connection->emit('READY', ['user' => ['id' => '1', 'username' => 'tenma'], 'application' => ['id' => 'app']]);

        $this->assertSame([], $connection->rest('GET', '/applications/app/commands'));
    }

    public function testAReturnedStringIsTheReply(): void
    {
        $this->bot->command('ping', 'Answers with pong', fn (Command $command) => 'pong');
        $this->connect();

        $this->connection->emit('INTERACTION_CREATE', self::interaction(2, ['name' => 'ping']));

        $this->assertSame(4, $this->callbacks()[0]['type']);
        $this->assertSame('pong', $this->callbackData()['content'] ?? null);
    }

    public function testAReplyAfterDeferReplacesTheThinkingMessage(): void
    {
        $this->bot->command('slow', 'Takes a while', function (Command $command) {
            $command->defer();
            return 'done';
        });
        $this->connect();

        $this->connection->emit('INTERACTION_CREATE', self::interaction(2, ['name' => 'slow']));

        $this->assertSame([5], array_column($this->callbacks(), 'type'));
        $edits = $this->connection->rest('PATCH', '/webhooks/app/t1/messages/@original');
        $this->assertSame('done', $edits[0]['content']);
    }

    public function testCommandOptionsReachTheHandler(): void
    {
        $seen = null;
        $this->bot->command('radio', 'Plays', function (Command $command) use (&$seen) {
            $seen = [$command->string('url'), $command->option('missing')];
            return null;
        });
        $this->connect();

        $data = ['name' => 'radio', 'options' => [['name' => 'url', 'type' => 3, 'value' => 'https://listen.moe/stream']]];
        $this->connection->emit('INTERACTION_CREATE', self::interaction(2, $data));

        $this->assertSame(['https://listen.moe/stream', null], $seen);
        $this->assertSame([], $this->callbacks());
    }

    public function testTheBotKnowsWhichVoiceChannelAUserSitsIn(): void
    {
        $voiceChannel = 'unset';
        $this->bot->command('radio', 'Plays', function (Command $command) use (&$voiceChannel) {
            $voiceChannel = $command->voiceChannel;
            return null;
        });
        $this->connect();

        $this->connection->emit('GUILD_CREATE', [
            'id' => 'g1',
            'name' => 'Guild',
            'voice_states' => [['user_id' => 'u1', 'channel_id' => 'v1']],
        ]);
        $this->connection->emit('VOICE_STATE_UPDATE', ['guild_id' => 'g1', 'user_id' => 'u1', 'channel_id' => 'v2']);
        $this->connection->emit('INTERACTION_CREATE', self::interaction(2, ['name' => 'radio']));
        $this->assertSame('v2', $voiceChannel);

        $this->connection->emit('VOICE_STATE_UPDATE', ['guild_id' => 'g1', 'user_id' => 'u1', 'channel_id' => null]);
        $this->assertNull($this->bot->voiceChannel('g1', 'u1'));
    }

    public function testAButtonIdWithAColonReachesTheHandlerOfItsPrefix(): void
    {
        $clicked = null;
        $this->bot->button('vote', function (Click $click) use (&$clicked) {
            $clicked = $click->buttonId;
            $click->update('Thanks');
            return null;
        });
        $this->connect();

        $this->connection->emit('INTERACTION_CREATE', self::interaction(3, ['custom_id' => 'vote:42']));

        $this->assertSame('vote:42', $clicked);
        $this->assertSame(7, $this->callbacks()[0]['type']);
    }

    public function testAFailingHandlerTellsTheUserAndStillThrows(): void
    {
        $this->bot->command('broken', 'Fails', fn (Command $command) => throw new \RuntimeException('boom'));
        $this->connect();

        try {
            $this->connection->emit('INTERACTION_CREATE', self::interaction(2, ['name' => 'broken']));
            $this->fail('The error was swallowed');
        } catch (\RuntimeException $error) {
            $this->assertSame('boom', $error->getMessage());
        }

        $this->assertSame(1 << 6, $this->callbackData()['flags'] ?? null);
    }

    public function testEventsArriveAsTheirClassAndAsDispatch(): void
    {
        $messages = [];
        $types = [];
        $this->bot->on(MessageCreated::class, function (MessageCreated $message) use (&$messages) {
            $messages[] = "{$message->author->name}: {$message->content}";
        });
        $this->bot->on(Dispatch::class, function (Dispatch $dispatch) use (&$types) {
            $types[] = $dispatch->type;
        });
        $this->connect();

        $this->connection->emit('MESSAGE_CREATE', [
            'id' => 'm1',
            'channel_id' => 'c1',
            'author' => ['id' => 'u1', 'username' => 'lento'],
            'content' => 'hello',
        ]);

        $this->assertSame(['lento: hello'], $messages);
        $this->assertSame(['READY', 'MESSAGE_CREATE'], $types);
    }

    public function testAnUnknownEventClassIsRefused(): void
    {
        $this->expectException(\InvalidArgumentException::class);
        $this->bot->on(UnknownEvent::class, fn (UnknownEvent $event) => null);
    }

    public function testAMessageCarriesEmbedsAndButtonsFiveToARow(): void
    {
        $buttons = array_map(fn (int $i) => new Button("b$i", "B$i"), range(1, 6));
        $message = new Message('hi', [new Embed(title: 'Title', fields: ['Now' => 'Song'])], $buttons, ephemeral: true);

        $array = $message->toArray();

        $this->assertSame([['title' => 'Title', 'fields' => [['name' => 'Now', 'value' => 'Song', 'inline' => true]]]], $array['embeds']);
        $this->assertIsArray($array['components']);
        $this->assertCount(2, $array['components']);
        $this->assertSame(1 << 6, $array['flags']);
    }

    public function testAnEditLeavesWhatTheMessageDoesNotSetAndClearsEmptyLists(): void
    {
        $this->assertSame(
            ['flags' => 0, 'content' => 'Muted', 'allowed_mentions' => ['parse' => []]],
            new Message('Muted')->toArray(),
        );
        $this->assertSame(
            ['flags' => 0, 'embeds' => [], 'components' => [], 'allowed_mentions' => ['parse' => []]],
            new Message(embeds: [], buttons: [])->toArray(),
        );
    }

    public function testEveryWayOfSendingPingsNobodyUnlessTheMessageAllowsIt(): void
    {
        $this->bot->command('echo', 'Echoes', function (Command $command) {
            $command->reply('@everyone');
            $command->followUp('<@u2>');
            $command->edit('<@&r1>');
            return null;
        });
        $this->bot->button('echo', function (Click $click) {
            $click->update('@here');
            return null;
        });
        $this->bot->on(MessageCreated::class, fn (MessageCreated $message) => $message->reply('@everyone'));
        $this->connection->responses['POST /channels/c1/messages'] = '{"id":"m2"}';
        $this->connection->responses['POST /channels/c2/messages'] = '{"id":"m3"}';
        $this->connect();

        $this->connection->emit('INTERACTION_CREATE', self::interaction(2, ['name' => 'echo']));
        $this->connection->emit('INTERACTION_CREATE', self::interaction(3, ['custom_id' => 'echo']));
        $this->connection->emit('MESSAGE_CREATE', [
            'id' => 'm1',
            'channel_id' => 'c1',
            'author' => ['id' => 'u1', 'username' => 'lento'],
            'content' => 'hello',
        ]);
        $this->bot->rest->send('c2', '@everyone');

        $bodies = [
            ...array_column($this->callbacks(), 'data'),
            ...$this->connection->rest('POST', '/webhooks/app/t1'),
            ...$this->connection->rest('PATCH', '/webhooks/app/t1/messages/@original'),
            ...$this->connection->rest('POST', '/channels/c1/messages'),
            ...$this->connection->rest('POST', '/channels/c2/messages'),
        ];
        $this->assertCount(6, $bodies);
        foreach ($bodies as $body) {
            $this->assertIsArray($body);
            $this->assertSame(['parse' => []], $body['allowed_mentions'] ?? null);
        }

        $allowed = new Message('<@u2>', mentions: ['users' => ['u2']])->toArray();
        $this->assertSame(['users' => ['u2'], 'parse' => []], $allowed['allowed_mentions']);
    }

    public function testTrackEventsReachTheirListeners(): void
    {
        $failed = null;
        $this->bot->on(TrackFailed::class, function (TrackFailed $track) use (&$failed) {
            $failed = $track;
        });
        $this->connect();

        $this->connection->emit('TRACK_FAILED', ['guild_id' => 'g1', 'url' => 'https://x/a.mp3', 'error' => 'not found']);

        $this->assertInstanceOf(TrackFailed::class, $failed);
        $this->assertSame(['g1', 'https://x/a.mp3', 'not found'], [$failed->guildId, $failed->url, $failed->error]);
    }

    public function testVoiceQueuesAndListsTracks(): void
    {
        $this->connection->responses['voice.tracks'] = '["https://x/a.mp3","https://x/b.mp3"]';
        $voice = $this->bot->voice('g1');

        $voice->queue('https://x/b.mp3');

        $this->assertSame(['https://x/a.mp3', 'https://x/b.mp3'], $voice->tracks());
        $calls = array_slice($this->connection->calls, -2);
        $this->assertSame(['voice.queue', 'voice.tracks'], array_column($calls, 'function'));
        $this->assertSame(['url' => 'https://x/b.mp3', 'guild' => 'g1'], $calls[0]['input']);
    }

    public function testAutocompleteSuggestsWhatTheHandlerReturns(): void
    {
        $stations = fn (Autocomplete $typed) => array_filter(
            ['J-pop' => 'https://listen.moe/stream', 'K-pop' => 'https://listen.moe/kpop/stream'],
            fn (string $name) => stripos($name, $typed->value) !== false,
            ARRAY_FILTER_USE_KEY,
        );
        $this->bot->command('radio', 'Plays', fn (Command $command) => null, [
            Option::string('url', 'Stream', autocomplete: $stations),
        ]);
        $this->connect();

        $data = ['name' => 'radio', 'options' => [['name' => 'url', 'type' => 3, 'value' => 'k-', 'focused' => true]]];
        $this->connection->emit('INTERACTION_CREATE', self::interaction(4, $data));

        $option = ['type' => 3, 'name' => 'url', 'description' => 'Stream', 'required' => false, 'autocomplete' => true];
        $put = $this->connection->rest('PUT', '/applications/app/commands');
        $this->assertSame([[['name' => 'radio', 'description' => 'Plays', 'options' => [$option]]]], $put);
        $this->assertSame(8, $this->callbacks()[0]['type']);
        $this->assertSame([['name' => 'K-pop', 'value' => 'https://listen.moe/kpop/stream']], $this->callbackData()['choices'] ?? null);
    }

    public function testAModalOpensAndItsAnswerReachesTheModalHandler(): void
    {
        $this->bot->command('suggest', 'Suggests a station', function (Command $command) {
            $command->modal(new Modal('station', 'Suggest a station', [new TextInput('url', 'Stream URL')]));
            return null;
        });
        $this->bot->modal('station', fn (Submit $submit) => "Thanks for {$submit->value('url')}");
        $this->connect();

        $this->connection->emit('INTERACTION_CREATE', self::interaction(2, ['name' => 'suggest']));
        $this->connection->emit('INTERACTION_CREATE', self::interaction(5, [
            'custom_id' => 'station',
            'components' => [['type' => 18, 'component' => ['type' => 4, 'custom_id' => 'url', 'value' => 'https://x/radio']]],
        ]));

        $callbacks = $this->callbacks();
        $this->assertSame([9, 4], array_column($callbacks, 'type'));
        $this->assertSame(
            ['flags' => 0, 'content' => 'Thanks for https://x/radio', 'allowed_mentions' => ['parse' => []]],
            $callbacks[1]['data'],
        );
    }
}
