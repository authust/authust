import axios from 'axios';
import { useRoute } from 'vue-router';
import type { FlowData } from './model';

export const axios_instance = axios.create({
    baseURL: 'http://127.0.0.1:8080/api/v1',
    withCredentials: true
})

function get_query() {
    return new URLSearchParams(window.location.search)
}

export function get_user_info() {}

export function execute_flow(flow_slug: string) {
    return axios_instance.get<FlowData>("/flow/executor/" + flow_slug, {
        params: {
            query: get_query()
        }
    })
}

export function execute_flow_post(flow_slug: string, form: HTMLFormElement) {
    let form_data = new FormData(form);
    // @ts-expect-error
    form_data = new URLSearchParams(form_data);
    return axios_instance.post<FlowData>("/flow/executor/" + flow_slug, form_data, {
        params: {
            query: get_query()
        }
    })
}