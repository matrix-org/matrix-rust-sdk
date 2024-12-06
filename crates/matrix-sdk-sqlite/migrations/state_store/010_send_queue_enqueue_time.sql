-- Migration script to add the enqueue_time column to the send_queue_events table
ALTER TABLE "send_queue_events"
ADD COLUMN "enqueue_time" INTEGER NOT NULL DEFAULT (strftime('%s', 'now'));

ALTER TABLE "dependent_send_queue_events"
ADD COLUMN "enqueue_time" INTEGER NOT NULL DEFAULT (strftime('%s', 'now'));
